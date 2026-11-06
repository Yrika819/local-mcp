# Security V3 — Spawn-Time Executable Identity & Trusted Install Root

Status: **DESIGN / NOT IMPLEMENTED** (2026-11-04). This document proposes the
Security V3 closure program for the helper-execution TOCTOU window left open
by Security V2. It authorizes no runtime changes. Any implementation requires
the Owner approvals listed in §12 and must land as contract-compatible,
fail-closed slices behind the existing Install Contract V1 selector.

Base: `main` at
`e6950fdc3bab72982285888a81cce822743fbba5` (re-synced from the original
`ea50127e13dc0d24e3b7bc3057690c5f8cca12c1` v0.1.2 base; the re-sync brings in
Setup Preview V1 and test stabilization, none of which this design depends
on or modifies).
Companion documents: `docs/SECURITY_V3_THREAT_MODEL.md`,
`docs/SECURITY_V3_TEST_PLAN.md`.

## 1. Problem statement

Security V2 (v0.1.2) rejects a helper whose **final entry** is a symlink
(`codex-linux-sandbox`) or a Windows reparse point (`atomic-publish`, every
platform). It does not bind the verified metadata to the image the OS opens.
Concretely, today's sequence:

1. runtime resolves the helper path as a sibling of `current_exe()`;
2. `symlink_metadata` checks the entry is a regular, non-link file;
3. `Command::new(path)` re-resolves the path, possibly through changed
   ancestors, and the kernel opens *whatever* is at that path at that moment.

Steps 2 and 3 are not atomic with respect to the filesystem. An attacker able
to write the helper's directory — or the helper itself between check and
spawn — can cause attacker-controlled code to run with the user's privileges.
The V2 audit recorded this as Case B/E, severity High, and Full Execution
Identity Closure as **NOT achieved**. Security V3 closes (or honestly
bounds) that gap.

## 2. Non-negotiable design rule

> **No design may claim that comparing a hash or metadata at check time
> guarantees the identity of the image the OS opens later.** Any such design
> treats an inherently racy comparison as an authorization. Security V3 only
> adopts designs in which either (a) verification and execution operate on
> the *same opened object* (kernel-pinned), or (b) the residual race is
> explicitly enumerated, fail-closed, and accepted by the Owner in writing.

Timing comparisons ("we checked 5 ms before spawn"), double-stat
re-verification ("stat before and after"), and mtime/ctime heuristics are
**not** identity binding and are rejected as closure mechanisms. They may
appear only as best-effort detection with the race acknowledged.

## 3. Scope

Focus areas (exactly the campaign scope):

1. Helper check-to-open TOCTOU
2. Spawn-time executable identity
3. Ancestor directory redirection
4. Trusted install root
5. Runtime / Validator contract alignment
6. Windows host-native execution boundary

Non-goals (this document authorizes none of these):

- No runtime implementation in this document's PR.
- No weakening or bypass of existing checks; every new failure mode is
  fail-closed.
- No new public CLI surface; Doctor additions are specified but gated.
- No change to archive formats, release assets, or tag/release process.
- No claim of same-token malware isolation (out of scope, unchanged).

## 4. Platform feasibility matrix

| Mechanism | Linux | macOS | Windows |
| --- | --- | --- | --- |
| fd-pinned exec (`execveat(AT_EMPTY_PATH)`) | Yes, kernel ≥ 5.6 with `openat2(RESOLVE_BENEATH/NO_SYMLINKS)`; glibc/musl exported | Partial: no `execveat`; `/dev/fd/N` exec path exists but needs native spike validation | No equivalent public API |
| dirfd-relative component walk with `O_NOFOLLOW`/`O_DIRECTORY` | Yes | Yes | Partial via `CreateFileW(FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS)` for detection only; pinning a directory identity does not pin a later path resolution by the OS |
| `fexecve` | Yes (glibc) | No fexecve; `/dev/fd` fallback | No |
| hash-at-open (streamed digest of the fd) | Yes, but this authenticates the *content read this run*, not the mapping used by exec; acceptable only combined with fd-pinned exec | Same | Same; and exec itself remains path-based |
| ACL/ownership enforcement at install time | mode/ownership per POSIX | POSIX mode + **mandatory** NFSv4-style ACL evaluation via the documented `ACL_TYPE_EXTENDED` API family (native behavior NOT VERIFIED until the D-V3-7 spike; fail-closed on retrieval failure) | DACL + owner-SID evaluation via `windows-sys` (dependency already present) |
| Atomic updater rename | rename(2) within install fs | same | MoveFileEx within same volume |
| Job containment of spawned helper | n/a (authority model) | n/a | unchanged; Windows host-native, no sandbox claim |

Reading: Linux can reach genuine spawn-time identity binding. macOS can
likely reach it via `/dev/fd` exec but requires a native spike before the
claim is made; otherwise the residual is documented and fail-closed. Windows
cannot reach it with public APIs; V3 defines detection, trusted-root
enforcement, and honest residuals instead.

## 5. Design decisions

### D-V3-1 — Open-then-exec is the only identity model
The launch path must:

1. open the install root and each ancestor level with
   `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` (Unix) /
   `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS` (Windows),
   verifying at each level from the **fd itself** (`fstat`/handle metadata —
   never a path re-stat): regular/directory type, no reparse/symlink where
   policy forbids it, device and inode identical to the values observed when
   the level was opened (crossing a *legitimate* mount boundary is handled by
   treating each mount as a new level with its own trusted-root evaluation,
   not by accepting an `EXDEV` silently), filesystem type via `fstatfs`
   (unrecognized or network filesystem → UNKNOWN, see §D-V3-4),
   ownership/mode within the trusted-root policy (§D-V3-4);
2. open the helper **relative to the pinned root dirfd** with
   `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
   O_PATH | O_CLOEXEC)` (Linux), `openat(O_RDONLY | O_NOFOLLOW |
   O_CLOEXEC)` (macOS — a **readable** fd is required because the `/dev/fd`
   exec path reads the file; `O_PATH` does not exist on macOS, and
   `O_SYMLINK` is a different bit with different semantics and is **not**
   used), or `CreateFileW` with `FILE_FLAG_OPEN_REPARSE_POINT` (Windows,
   detection-only);
3. `fstat` the resulting fd/handle: regular file, expected owner, expected
   mode, single-link or recorded inode;
4. optional content digest **over the fd** (install-time only; never the
   per-spawn path unless policy demands it);
5. spawn from the pinned object: `execveat(dirfd_or_empty, "", ...)` with
   `AT_EMPTY_PATH` (Linux, native binary — see the scope note below),
   `execve("/dev/fd/<n>", ...)` (macOS, **NOT VERIFIED** — gated on the
   D-V3-7 spike; strict mode must not be enabled on macOS before the spike
   passes), or, on Windows, `CreateProcessW` with `lpApplicationName` set to
   the **absolute verified path** (so the child parse cannot split or
   reinterpret the executable token), `lpCommandLine` built with the
   standard Windows quoting convention for the verified path plus
   arguments, `bInheritHandles` pinned to an explicit caller-chosen value
   (default FALSE), and explicit `dwCreationFlags` (documented `CREATE_*`
   set only; no flag that would relax the verification context). Windows
   spawn is path-based and claims no identity binding (D-V3-6);
6. if any step fails, or the OS refuses fd-exec (SELinux, LSM, binfmt), the
   helper does not run; the failure is explicit (Doctor: BLOCK/UNKNOWN).

An inode pinned by an fd cannot be swapped by renaming. In-place writes to a
pinned inode still change its bytes — that is unavoidable and is stated, not
hidden (threat T-V3-4). The guarantee is precisely: *on Linux, the native
binary executed is the verified inode* (via `execveat(AT_EMPTY_PATH)`), not
"the bytes are authenticated forever". The claim is scoped to fd-pinned
platforms and to executables the kernel runs directly from the pinned fd:
**script files are out of closure scope** — an interpreter re-opens the
script by path (shebang re-resolution), so a script helper carries a
path-based residual even on Linux and must never be claimed as
fd-pinned. Windows is exempt entirely (`CreateProcessW` is path-based).
macOS: verification and execution remain two separate steps (verify the
opened fd, then `execve("/dev/fd/N")`) — this does **not** by itself close
the TOCTOU window; closure is only claimable after the D-V3-7 spike proves
`/dev/fd/N` exec semantics natively, and until then macOS carries the
documented residual, never a strict-mode claim.

Two related honest boundaries: (a) a *separate verification fd* plus a
*separate execution open* is a double-open, not an atomic identity bind —
the macOS model must either prove the single-fd `/dev/fd` exec path or carry
the residual; (b) verification of the fd and use of the fd by the kernel
must be distinguished in every platform claim: only Linux currently achieves
"the kernel executes the very fd that was verified".

### D-V3-2 — Parent-directory pinning beats final-entry checks
The ancestor walk (D-V3-1 step 1) is the control for redirection attacks.
Replacing the root directory, an intermediate directory, or the mount makes
the walk fail closed (device/inode mismatch between `fstat(fd)` at open time
and `fstat(fd)` of the same open fd after each subsequent level, `EXDEV` at
the next `openat` relative to a pinned dirfd, `fstatfs` type mismatch,
inode change). Legitimate mount boundaries (e.g., the install tree crossing
into a local bind/data mount) are supported only as *newly evaluated levels*
under the same trusted-root policy; there is no blanket `EXDEV` bypass.
Final-entry symlink rejection remains, as defense in depth, but
moves from "the control" to "one control among several".

### D-V3-3 — Runtime and Validator share one decision table
`is_plain_file`/`first_existing` stay the only shared rules; V3 re-specifies
the shared decision as a **single normative decision table** (verdict per
observation row, defined in this document and the Test Plan) with **two
independent implementations** — the Rust runtime side and the Python
Install Contract validator side — plus a planned third consumer (Doctor,
NOT IMPLEMENTED and excluded from parity claims until it exists). The
wording "one shared function" is intentionally retired: there is no single
function across Rust and Python, and pretending otherwise would over-claim
parity.

The parity claim is bounded:

- parity tests assert decision identity between the Rust and Python
  implementations over the **shared deterministic fixture matrix** (Test
  Plan §4/§6);
- kernel/capability branches (e.g., `openat2` availability, SELinux refusal,
  Windows privilege holders) are **outside** the parity claim — they are
  platform facts, reported per leg;
- Doctor is marked NOT IMPLEMENTED and is excluded from the parity claim
  until its V3 probes exist (D-V3-5);
- any divergence on the shared matrix is a BLOCK, and the decision is
  fail-closed by construction: unknown capability → BLOCK/UNKNOWN, never
  PASS.

Consumers of the verdict:

- the runtime selector (execution path),
- the Install Contract validator (`scripts/release/install_contract.py`
  parity tests),
- Doctor (classification only; Doctor never enforces; NOT IMPLEMENTED),
- future Setup (install-time gate).

### D-V3-4 — Trusted install root is a policy, evaluated per OS
A path is trusted for helper execution only if:

- **Linux**: every ancestor from the filesystem boundary to the install
  directory is owned by root (or the documented installer uid) with mode
  `0755` or stricter, and no group/other write bit; the install directory
  itself is owned by root/installer, `0755` or stricter; no ancestor is a
  symlink/remount-to-untrusted-fs; filesystem type is on an explicit
  allowlist (`fstatfs`), and unrecognized or network filesystems are UNKNOWN
  unless an explicit Owner attestation is recorded.
- **macOS**: the same POSIX ownership/mode rules as Linux, **plus** an
  evaluation of NFSv4-style extended ACLs (Apple `ACL_TYPE_EXTENDED`,
  retrieved with the documented `acl_get_file`/`acl_get_entry` API family)
  on every ancestor and the install directory. POSIX mode bits do not
  reflect ACL `allow` entries: a directory whose group bits look clean can
  still grant write access through an inherited ACL entry. The policy is:
  - an ACL `allow` entry granting write to a principal other than
    root/installer → BLOCK;
  - inherited ACL entries are evaluated as their own findings (an inherited
    allow from a trusted ancestor may be accepted only if the ancestor
    itself passed the same policy);
  - Group/Everyone/`everyone`/`group` write capability, whether from mode or
    ACL → BLOCK;
  - parent-directory ACLs are part of the walk and evaluated identically;
  - if ACLs cannot be retrieved or parsed (unsupported filesystem, API
    error) → UNKNOWN, fail-closed — never a silent POSIX-only downgrade;
  - if native validation of the ACL API behavior on the target filesystems
    has not been performed, the entire macOS ACL evaluation is recorded as
    **NOT VERIFIED** and strict mode must not be enabled on macOS (see
    D-V3-7 and Test Plan A-M5). The Owner may explicitly accept the
    remaining risk of an unevaluated-ACL environment in writing; that
    acceptance is recorded next to this paragraph, never implied.
- **Windows**: every ancestor and the install directory must pass BOTH an
  owner check AND a DACL check; either alone is insufficient. Evaluation is
  via `GetSecurityInfo`/`GetNamedSecurityInfo` (`OWNER_INFORMATION`,
  `DACL_SECURITY_INFORMATION`), with `GetSecurityDescriptorOwner`/
  `GetSecurityDescriptorDacl` on the resulting descriptor:
  - **Owner SID** must resolve to a member of { Administrators, SYSTEM,
    documented installer SID }. Rationale: a Windows object owner holds
    implicit `READ_CONTROL`/`WRITE_DAC` on the object regardless of the
    DACL contents, so a "clean-looking" DACL owned by an attacker-controlled
    principal is bypassable; owner and DACL are independent gates and both
    must pass;
  - **DACL** allow-set must contain only installer/Administrators/SYSTEM
    (plus read/execute for Users), evaluated over inherited AND explicit
    ACEs (inherited ACEs must be traced to trusted parents — an inherited
    allow from a rejected parent is a finding); any explicit Deny that
    contradicts policy → BLOCK; any write-capable ACE for
    Users/Everyone/Authenticated Users → BLOCK;
  - the effective writability of every directory is evaluated as "which
    principals could modify entries or files", combining owner rights, ACE
    rights, and integrity-level considerations; if that combination cannot
    be computed confidently → UNKNOWN;
  - no ancestor is a reparse point (junction/symlink mount) except where
    policy explicitly allows (e.g., Nix); any reparse ancestor → BLOCK
    (or UNKNOWN if the reparse data cannot be read);
  - if the security descriptor is missing, unparsable, or retrieval is
    denied → **UNKNOWN**, fail-closed — never allow-by-default;
  - volume must be local or explicitly attested; unknown filesystems are
    UNKNOWN.
  Windows remains **Experimental** with no Unix-equivalent sandbox or
  identity guarantee; these checks are trusted-root *preconditions*, not a
  claim of spawn-time identity (D-V3-6).

Feasibility: Unix ownership/mode checks are directly implementable. Windows
DACL/owner parsing is implementable with `windows-sys` (already a dependency)
but must be bounded and fail-closed on parse ambiguity. macOS ACL evaluation
requires the native spike (D-V3-7) before its behavior is trusted. Where
evaluation is impossible at runtime (e.g., network mounts without
attestation, unparsable descriptors), the verdict
is UNKNOWN → blocks install authorization, allows nothing silently.

### D-V3-5 — Doctor reporting contract (future slices, designed now)
If/when Doctor gains V3 checks, the contract is:

- new checks are `requirement: optional` for ordinary MCP use and required
  for install authorization;
- absence of the V3 probe reports `UNKNOWN` with `not_run` evidence — never
  PASS;
- Doctor remains non-enforcing and side-effect-free;
- the preview (Setup Preview V1) may display V3 verdicts as diagnostic
  content only; they are not install authorizations.

### D-V3-6 — Windows stays Experimental; identity binding is not claimed
Windows has no stable public API to create a process from an image handle.
V3 therefore specifies for Windows:

- DACL/owner/ancestor/reparse *detection* at verify time (D-V3-1 steps 1–3;
  owner-SID and DACL evaluation per D-V3-4),
- immediate pre-spawn re-verification as a race *detector* (never as an
  authorization),
- Job Object containment (existing behavior, unchanged),
- explicit residual: the path-based `CreateProcessW` re-parse window is
  **accepted and documented**, with the trusted-install-root precondition as
  the only real control;
- classification remains **Experimental**; sandbox-equivalent claims are
  forbidden; CLI/docs must not imply otherwise.

If the Owner requires spawn-time identity on Windows, the design space is:
undocumented `NtCreateUserProcess` internals (rejected — unstable,
unsupportable), an AppContainer broker as the launcher (architecture change,
out of V1 scope), or OS-level code-integrity policy (WDAC/AppLocker — a
deployment control, not a code change). V3 recommends documenting the
residual and revisiting after AppContainer broker design exists.

### D-V3-7 — macOS spike is mandatory before closure claims
`execve("/dev/fd/N")` on macOS:

- historically works for regular files open for reading; the design models
  the macOS launch as `openat(O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` → `fstat`
  verification → `execve("/dev/fd/N")`, i.e. a readable fd (no `O_PATH` —
  it does not exist on macOS; `O_SYMLINK` is a different bit and is not
  used);
- must be validated natively on Apple Silicon and Intel runners, including
  interaction with SIP, code signature validation, and notarization stubs,
  and specifically including whether the kernel executes the very file the
  fd refers to (identity binding) or re-opens `/dev/fd/N` by a mechanism
  that can race — the spike must distinguish these two behaviors, because
  only the first closes the TOCTOU window;
- the spike scope additionally covers the D-V3-4 macOS ACL evaluation
  (`ACL_TYPE_EXTENDED` retrieval and semantics on APFS, including inherited
  entries) and the POSIX-mode interaction — **NOT VERIFIED** until then;
- if the spike passes, macOS reaches Linux-equivalent closure for the exec
  step (with ancestor verification identical);
- if not, macOS ships with the D-V3-6-style residual (detection + trusted
  root) and the limitation is documented, never silently.

No closure statement for macOS is valid without the spike's native evidence.
Absent native evidence, every macOS-specific guarantee in this design is
recorded **NOT VERIFIED** and strict mode stays disabled on macOS.

### D-V3-8 — Install-time sealing is part of the design, not an add-on
Installers (and the future Setup) must, in the same transaction that places
helpers:

- set ownership/mode per D-V3-4;
- apply `chattr +i`/ACL denial as optional hardening where the admin opts in;
- re-verify the sealed result before first use;
- record (but never depend on) a digest manifest for audit — the manifest is
  evidence for Doctor/validator, not a runtime gate.

### D-V3-9 — Compatibility with Install Contract V1 and Security V2
- Archive names, layout, sibling rule, plain-file rule: unchanged.
- Existing negative tests remain valid; V3 tests are additive.
- Any tightening that rejects previously-accepted layouts (e.g., a helper
  resolved through a user-writable ancestor) is a **contract major bump**
  with migration notes; it must not ship silently. V1 behavior in V1-built
  binaries stays as-is; V3 lands as opt-in strict mode first, then, after
  Owner review, as default.
- Doctor PASS remains non-authorizing; nothing here creates an install
  authorization.

### D-V3-10 — Failure semantics: fail-closed everywhere
Any verification error → the helper does not execute → the caller surfaces
BLOCK/UNKNOWN. There is no "best effort" execution path, no retry with
weaker checks, no fallback to V1 behavior inside a V3-enabled strict path.
Availability impact is accepted as the cost of integrity; it is surfaced, not
smoothed over.

## 6. Verification and acceptance criteria (per OS)

A V3 implementation may claim closure per platform only when all apply:

**Linux**
- AC-L1: native CI demonstrates fd-pinned exec refuses or binds under
  ancestor-swap, final-name-swap, and in-place byte-edit fixtures
  (N-V3-5 shape; mtime is never treated as identity evidence).
- AC-L2: in-place byte modification between open and exec executes the
  *pinned inode*; test proves no path-based re-resolution occurs (fault
  injection via a wrapper path whose target changes after open).
- AC-L3: `openat2` absence (older kernels/libcs) yields explicit, documented
  refusal or Owner-accepted fallback — never silent weakening.
- AC-L4: performance: no measurable regression beyond a per-spawn overhead
  budget agreed by the Owner (proposal: ≤ 1 ms median on the *release-leg
  CI runner class* — the concrete runner image used for release
  verification, medians over ≥ 1,000 spawns per N-V3-12; the budget must be
  re-agreed for any other host class before a claim is extended to it;
  measure, never assert blindly).
- AC-L5: Bubblewrap gate compatibility: helpers spawned inside the sandbox
  derive from the same pinned objects; no descriptor leaks another helper's
  fd into the child without intent.

**macOS**
- AC-M1: the `/dev/fd` exec spike passes on both runners, or the limitation
  is documented with Owner sign-off (then AC-M2/M3 apply instead).
- AC-M2: ancestor walk with `O_NOFOLLOW` passes the redirection matrix.
- AC-M3: residual risk text ships in docs and Doctor messaging if the spike
  fails; classification wording reviewed by Owner.

**Windows**
- AC-W1: DACL verification passes the allow/deny matrix natively;
  inconclusive DACLs yield UNKNOWN (fail closed).
- AC-W2: the re-parse residual is present verbatim in docs, Doctor
  messaging, and release notes.
- AC-W3: no claim of sandbox parity or of spawn-time identity is present
  anywhere in shipped text.

**Cross-platform**
- AC-X1: shared decision parity (runtime vs validator vs Doctor) passes on
  all CI legs.
- AC-X2: full fail-closed matrix (every verification failure → no exec →
  explicit verdict) passes on all legs.
- AC-X3: existing security gates (Install Contract V1, Doctor schema/producer
  validation, schema CI) remain green.
- AC-X4: performance impact is reported with measurements, not estimates.

## 7. Migration and rollout

1. Slice A: shared `HelperTrust` verdict + parity harness (no behavior
   change; verdict computed alongside, logged to tests only).
2. Slice B: strict-mode fd-pinned exec on Linux behind
   `LOCAL_MCP_HELPER_IDENTITY=strict` (default off), with native CI.
3. Slice C: macOS spike + (on pass) strict mode; default unchanged.
4. Slice D: Windows DACL verification behind the same strict flag.
5. Slice E: Owner decision review → default-on per OS, docs/release notes,
   Install Contract major notes if any tightening shipped.

Each slice is independently revertable; strict mode never activates without
the flag; no slice removes an existing check.

## 8. Performance budget

- Unix strict open: one root open + depth ancestor opens + one helper
  `openat2` + `fstat`: O(depth) syscalls, no heap churn on hot path expected
  to be < 0.5 ms; measured per AC-L4/X4 before default-on.
- No per-spawn hashing; digest verification occurs at install time and in
  audit tooling only (hash-at-exec would not bind the image anyway).
- Windows: DACL/owner evaluation per verify (not per spawn on hot path).
  If an evaluation cache is introduced, it is valid **only** for the
  verify-then-spawn window of the current launch: max-age 0 (single use) or
  invalidation by a change notification for the exact directory, whichever
  is implemented; a cache that cannot prove invalidation is forbidden, and
  any cache miss or expiry → UNKNOWN, never a stale PASS.
- No background threads, no persistent fds.

## 9. Failure semantics summary

| Failure | Verdict | Effect |
| --- | --- | --- |
| ancestor not a directory | BLOCK | no exec |
| ancestor dev change during walk | BLOCK | no exec |
| ancestor reparse/symlink (Windows) | BLOCK (or UNKNOWN if unparsable) | no exec |
| ancestor symlink where `O_NOFOLLOW` forbids (Unix) | BLOCK | no exec |
| ancestor ownership/mode/ACL out of policy (Unix) | BLOCK | no exec |
| Unix ancestor ACL unparsable (macOS) | UNKNOWN | no exec |
| helper missing | BLOCK (existing) | no exec |
| helper not regular / link | BLOCK (existing) | no exec |
| fd-exec unsupported | UNKNOWN (strict) | no exec |
| DACL unparsable | UNKNOWN | no exec |
| Windows owner SID outside policy or unresolvable | UNKNOWN | no exec |
| Windows security descriptor missing/unreadable | UNKNOWN | no exec |
| TOCTOU re-check mismatch (Windows) | UNKNOWN | no exec |

## 10. Limitations (explicit)

- Same-token malware is out of scope and remains NO-GO for install
  authorization.
- In-place writes to a pinned inode change its bytes; V3 does not attempt to
  prevent an authorized writer from modifying what they may write. It binds
  *identity*, not *immutability*. Immutability is the trusted-root policy's
  job.
- Atomic updater races (Case D) become deterministic at exec time **on
  fd-pinned platforms (Linux; macOS only if the D-V3-7 spike passes)**: the
  pinned image runs or the launch fails. Windows has no fd-pinned exec, so
  no determinism is claimed there — the only real control is the
  trusted-install-root precondition (D-V3-4/D-V3-6), and the re-parse
  residual stands. Which generation
  wins is still updater-policy, not security; see Threat Model T-V3-7.
- Network filesystems and user-writable ancestors are NO-GO unless attested.
- This document does not certify any deployment. Deployments are trusted by
  construction (D-V3-4), and the runtime cannot prove that construction
  portably — it can only check what it is taught to check and fail closed.

## 11. Open questions for the Owner

1. Approve Linux strict mode as the default for v0.2.x, or keep opt-in?
2. Authorize the macOS `/dev/fd` spike cost on both runner types?
3. Approve the Windows residual documentation wording (Experimental, no
   identity binding) for inclusion in release notes and Doctor output?
4. Approve contract-major tightening (user-writable ancestor rejection) or
   keep as strict-flag behavior?

## 12. Owner approvals required before implementation

- [ ] Approval of this design and the companion threat model/test plan.
- [ ] Approval of strict-mode rollout policy (§7, §11).
- [ ] Approval of the macOS spike and its failure contingency.
- [ ] Approval of the Windows residual risk wording.
- [ ] Approval of any Install Contract major-note tightening before release.
