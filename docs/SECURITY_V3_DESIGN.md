# Security V3 — Spawn-Time Executable Identity & Trusted Install Root

Status: **DESIGN / NOT IMPLEMENTED** (2026-11-04). This document proposes the
Security V3 closure program for the helper-execution TOCTOU window left open
by Security V2. It authorizes no runtime changes. Any implementation requires
the Owner approvals listed in §12 and must land as contract-compatible,
fail-closed slices behind the existing Install Contract V1 selector.

Base: `main` at `ea50127e13dc0d24e3b7bc3057690c5f8cca12c1` (v0.1.2).
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
| ACL/ownership enforcement at install time | mode/ownership per POSIX | mode/ownership per POSIX + ACL via `osx-acl`/`listxattr` (optional) | DACL parsing via `windows-sys` (dependency already present) |
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

1. open the install root and each ancestor with `O_DIRECTORY` (Unix) /
   `FILE_FLAG_OPEN_REPARSE_POINT` (Windows), verifying at each level:
   regular/directory type, no reparse/symlink where policy forbids it,
   mount identity (`st_dev`) stable, ownership/mode within the trusted-root
   policy (§D-V3-4);
2. open the helper **relative to the pinned root dirfd** with
   `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS, O_PATH)` (Linux),
   `openat(O_NOFOLLOW | O_PATH)` (macOS), or `CreateFileW` with
   `FILE_FLAG_OPEN_REPARSE_POINT` (Windows, detection-only);
3. `fstat` the resulting fd/handle: regular file, expected owner, expected
   mode, single-link or recorded inode;
4. optional content digest **over the fd** (install-time only; never the
   per-spawn path unless policy demands it);
5. spawn from the pinned object: `execveat(dirfd_or_empty, "", ...)` with
   `AT_EMPTY_PATH` (Linux), `execve("/dev/fd/<n>", ...)` (macOS, post-spike),
   or `CreateProcessW(escaped_path)` (Windows — no identity binding; see
   D-V3-6);
6. if any step fails, or the OS refuses fd-exec (SELinux, LSM, binfmt), the
   helper does not run; the failure is explicit (Doctor: BLOCK/UNKNOWN).

An inode pinned by an fd cannot be swapped by renaming. In-place writes to a
pinned inode still change its bytes — that is unavoidable and is stated, not
hidden (threat T-V3-4). The guarantee is precisely: *the image executed is the
inodular object verified*, not "the bytes are authenticated forever".

### D-V3-2 — Parent-directory pinning beats final-entry checks
The ancestor walk (D-V3-1 step 1) is the control for redirection attacks.
Replacing the root directory, an intermediate directory, or the mount makes
the walk fail closed (`st_dev` change, `EXDEV`, pipe-to-moved-mount, inode
change). Final-entry symlink rejection remains, as defense in depth, but
moves from "the control" to "one control among several".

### D-V3-3 — Runtime and Validator share one decision function
`is_plain_file`/`first_existing` stay the only shared rules; V3 wraps them in
a single `verify_helper_chain(...) -> HelperTrust` verdict consumed by:

- the runtime selector (execution path),
- the Install Contract validator (`scripts/release/install_contract.py`
  parity tests),
- Doctor (classification only; Doctor never enforces),
- future Setup (install-time gate).

Any divergence between these four is a BLOCK, and a parity test asserts
identity of decisions over the shared matrix (Test Plan §4). The decision is
fail-closed by construction: unknown capability → BLOCK/UNKNOWN, never PASS.

### D-V3-4 — Trusted install root is a policy, evaluated per OS
A path is trusted for helper execution only if:

- **Linux/macOS**: every ancestor from the filesystem boundary to the install
  directory is owned by root (or the documented installer uid) with mode
  `0755` or stricter, and no group/other write bit; the install directory
  itself is owned by root/installer, `0755` or stricter; no ancestor is a
  symlink/junction/remount-to-untrusted-fs; network filesystems require an
  explicit Owner attestation.
- **Windows**: every ancestor and the install directory carry a DACL whose
  allow-set contains only the installer/Administrators/SYSTEM (plus
  read/execute for Users), with no deny entries that contradict policy and no
  write ACE for Users/Everyone; no ancestor is a reparse point except where
  policy explicitly allows (e.g., Nix); volume is local or attested.

Feasibility: Unix ownership/mode checks are directly implementable. Windows
DACL parsing is implementable with `windows-sys` (already a dependency) but
must be bounded and fail-closed on parse ambiguity. Where evaluation is
impossible at runtime (e.g., network mounts without attestation), the verdict
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

- DACL/ancestor/reparse *detection* at verify time (D-V3-1 steps 1–3),
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

- historically works for regular files open for reading;
- must be validated natively on Apple Silicon and Intel runners, including
  interaction with SIP, code signature validation, and notarization stubs;
- if the spike passes, macOS reaches Linux-equivalent closure for the exec
  step (with ancestor verification identical);
- if not, macOS ships with the D-V3-6-style residual (detection + trusted
  root) and the limitation is documented, never silently.

No closure statement for macOS is valid without the spike's native evidence.

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
  ancestor-swap, final-name-swap, and in-place-mtime-change fixtures.
- AC-L2: in-place byte modification between open and exec executes the
  *pinned inode*; test proves no path-based re-resolution occurs (fault
  injection via a wrapper path whose target changes after open).
- AC-L3: `openat2` absence (older kernels/libcs) yields explicit, documented
  refusal or Owner-accepted fallback — never silent weakening.
- AC-L4: performance: no measurable regression beyond a per-spawn overhead
  budget agreed by the Owner (proposal: ≤ 1 ms median on the reference
  hardware; measure, never assert blindly).
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
- Windows: DACL evaluation per verify (cached per directory with explicit
  invalidation), not per spawn on hot path.
- No background threads, no persistent fds.

## 9. Failure semantics summary

| Failure | Verdict | Effect |
| --- | --- | --- |
| ancestor not a directory | BLOCK | no exec |
| ancestor dev change during walk | BLOCK | no exec |
| ancestor reparse/symlink (Windows) | BLOCK (or UNKNOWN if unparsable) | no exec |
| helper missing | BLOCK (existing) | no exec |
| helper not regular / link | BLOCK (existing) | no exec |
| fd-exec unsupported | UNKNOWN (strict) | no exec |
| DACL unparsable | UNKNOWN | no exec |
| TOCTOU re-check mismatch (Windows) | UNKNOWN | no exec |

## 10. Limitations (explicit)

- Same-token malware is out of scope and remains NO-GO for install
  authorization.
- In-place writes to a pinned inode change its bytes; V3 does not attempt to
  prevent an authorized writer from modifying what they may write. It binds
  *identity*, not *immutability*. Immutability is the trusted-root policy's
  job.
- Atomic updater races (Case D) become deterministic at exec time with
  fd-pinning: the pinned image runs or the launch fails. Which generation
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
