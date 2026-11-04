# Security V3 — Test Plan

Status: **DESIGN-LEVEL / NOT IMPLEMENTED** (2026-11-04). This plan is
normative for any Security V3 implementation PR. It assumes the threat model
and design documents in this directory. No test in this plan may be weakened,
skipped, or converted to PASS when its precondition is absent; absent
preconditions are reported as **NOT RUN** with a reason.

## 1. Test levels

| Level | Scope | Runner |
| --- | --- | --- |
| Unit (pure) | verdict table, ancestor metadata model, DACL parser bounds | `cargo test` |
| Fixture (native) | fd-pinned open/exec, ancestor swap, symlink/junction matrix | `cargo test` + native CI |
| Parity | runtime vs validator vs Doctor decision identity over one matrix | `cargo test` + Python validator parity script |
| Chaos/injection | fault injection around open/exec ordering | native CI (opt-in) |
| Perf | per-spawn overhead measurement | bench harness, reported not asserted |

Global rules:

- Test-owned fixtures only; never touch the operator's install or state.
- No test performs network I/O.
- Helper-execution tests never run attacker-controlled payloads as more
  than a no-op sentinel (e.g., exit code 42) inside test-owned directories.
- Linux/macOS results are never substituted for by Windows results and vice
  versa; each OS runs its own matrix.
- "NOT RUN" is an explicit ledger entry, never silently absent.

## 2. Core negative matrix (blocking for any closure claim per OS)

| ID | Fixture | Expected (strict mode) | NOT RUN condition |
| --- | --- | --- | --- |
| N-V3-1 | Final entry is a symlink/junction/reparse to a regular file | helper refused; verdict BLOCK; unchanged behavior vs V2 | none |
| N-V3-2 | Helper replaced *after* open, before exec (rename-over) | pinned inode executes or exec fails; swapped bytes never run | non-fd-exec平台はstrict claim不可 → residual documented instead |
| N-V3-3 | Final filename replaced to point to attacker payload after open | pinned object executes; payload not reached | see N-V3-2 |
| N-V3-4 | Ancestor replaced/redirected: root renamed + recreated, junction flipped, mount stacked | walk fails (dev/inode mismatch); no exec; BLOCK/UNKNOWN | junction ops on Windows require symlink privilege; convert to NOT RUN otherwise |
| N-V3-5 | In-place byte edit of pinned helper pre-exec | deterministic behavior: the pinned inode's bytes at exec time run; document that byte freshness is NOT asserted; record observation | none |
| N-V3-6 | Doctor/validator given V3-refusable layout | shared verdict = BLOCK; Doctor reports context as diagnostic content only; no surface allows exec | none |
| N-V3-7 | Windows DACL unparsable (denied read on DACL) | UNKNOWN; no exec; never allow-by-default | ACL denial fixture requires elevated rights; NOT RUN with reason otherwise |
| N-V3-8 | Verdict matrix: runtime vs validator vs Doctor disagreement injected via fixture | parity test fails; merge blocked | none |
| N-V3-9 | Windows path re-parse race (file swapped between recheck and `CreateProcessW`) | detected by re-check comparison (best-effort) or residual hit; behavior is documented, never an integrity claim | cannot deterministically force the race on demand; test asserts detection of *content/identity* change via re-check, else NOT RUN |
| N-V3-10 | Helper fd inherited into unrelated child | child holds no unexpected fd; `O_CLOEXEC` asserted | none |
| N-V3-11 | Verdict function branch exhaustion | every branch fails closed; compiler-enforced exhaustiveness; each branch has a dedicated test | none |
| N-V3-12 | Perf: per-spawn overhead on the reference host | measured ≤ agreed budget (Design §8) or default-on is blocked | hardware variance: report medians over ≥ 1k spawns |

## 3. Linux-specific acceptance tests

- A-L1: `openat2` with `RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS` rejects
  escape attempts (`..`, symlink ancestors introduced mid-walk by fixture).
- A-L2: `execveat(AT_EMPTY_PATH)` on the helper `O_PATH` fd executes the
  pinned inode; swapping via rename after open does not change the executed
  image (measured via a sentinel `exit(42)` fixture vs attacker `exit(66)`).
- A-L3: on kernels without `openat2`, strict mode reports UNKNOWN/BLOCK per
  policy and never weakens to path-based exec. Use `unshare`-based or
  seccomp-restricted fixture hosts to emulate, else NOT RUN with reason.
- A-L4: fd-exec under the Bubblewrap gate: the sandbox bootstrap receives
  only intended fds; no fd leak amplification (inspect via `/proc/self/fd`
  in a test helper).
- A-L5: read-only install (`chown root`, mode `0555` tree) passes strict
  verification; a world-writable ancestor fails closed.

## 4. macOS-specific acceptance tests

- A-M1 (spike gate): `/dev/fd/N` exec works on Intel and Apple Silicon CI
  runners with the codesigned fixture helper; result recorded with exact
  commands and runner versions.
- A-M2: ancestor walk with `O_NOFOLLOW` + dev/inode stability across the
  full matrix of N-V3-4.
- A-M3: if spike fails: docs/Doctor messaging residual wording test (grep
  gate in CI for forbidden phrases like "spawn-time identity verified").
- A-M4: SIP-enabled host notes: verification remains possible; exec claim
  follows the spike result.

## 5. Windows-specific acceptance tests

- A-W1: DACL parser: allow-by-Admins/SYSTEM subprocess helper passes;
  Users-writable ancestor fails; Deny-Users entries honored; unparsable
  DACL → UNKNOWN (fail closed).
- A-W2: final reparse point rejected (existing); parent junction detected
  and rejected in strict mode (detection, not elimination claim).
- A-W3: residual wording presence test: docs and Doctor-facing strings
  contain the accepted limitation, never the forbidden positive claims.
- A-W4: Job containment unchanged; no sandbox claim text appears.
- A-W5: pre-spawn re-check catches *content* replacement when the fixture
  can force it (replace between check and spawn in a controlled loop;
  detection asserted, elimination not claimed).

## 6. Parity and contract tests

- P-1: the shared verdict table is asserted identical between runtime and
  the Install Contract validator for every row of the matrix (both are
  exercised; the Python side validates the same fixtures via the validator's
  own negative-test corpus).
- P-2: Doctor, when extended, reports the same verdict; a fixture agreeing
  with runtime but not validator (or vice versa) fails P-1.
- P-3: Install Contract V1 decision table is a subset (all V1 accepts
  remain accepts in V2 mode with same file classes) unless a
  contract-major note is explicitly issued; any tightening requires the
  major-note gate.
- P-4: existing Windows/Linux/macOS CI legs remain green; no leg is
  permitted to flip an installation from accepted to rejected outside the
  strict flag.

## 7. Release-blocking criteria

Strict mode may not ship as default on any OS until:

- its full N-V3-* and A-* matrices are green natively on that OS;
- parity tests P-1/P-2 pass on all legs;
- perf budget AC-L4/N-V3-12 is met and reported;
- the Owner has approved the rollout wording and the residual texts.

Until then, strict mode is opt-in only, and default behavior remains V2.

## 8. Evidence ledger

Every CI run for a V3 PR must publish (as artifacts or job summary):

- OS, runner, kernel/OS build, and privilege notes (symlink privilege on
  Windows);
- per-test result ∈ {PASS, FAIL, NOT RUN + reason};
- the strict-mode flag state and kernel support matrix;
- perf measurements (if applicable), raw, not summarized away.
