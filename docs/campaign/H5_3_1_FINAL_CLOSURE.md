# H5.3.1 — Final review closure (phase I of the integration campaign)

Campaign: GoalLatch External Agent Backend V1 — **Integration方式C**
Branch: `campaign/h5-3-1-final-closure-v1` (from PR #57 HEAD `b85355b`)
Worktree: `.local/int-h531-orchestrator`
`main` at the start of this phase: `e6950fdc3bab72982285888a81cce822743fbba5`

This document exists because the H5.3 review could not be closed on the final
HEAD. It records what was re-audited, what was measured on real hardware, what
was changed, and exactly what remains unproven.

---

## 1. The open judgment difference, and why it survived H5.3

| Round | HEAD | Reviewer verdict | What it meant |
| --- | --- | --- | --- |
| 5 | `f74b805` | `NOT_READY` (harness finding R1b) | A group/world-writable fixture root lets another user replace a fixture file with a link between the hazard scan and the write |
| — | `d3267bf` | not reviewed | POSIX: the root is created `0o700` and a shared root is refused |
| — | `b85355b` | not reviewed | Docs/evidence only |

`d3267bf` addressed R1b **only where the reviewer had looked**. The code still
opened with:

```python
if os.name == "nt":
    return          # _assert_private_fixture_root: no Windows check at all
...
root.mkdir()        # inherits the parent DACL on Windows
```

The H5.3 evidence recorded the assumption behind that: *"Windows `%TEMP%` is
already per-user"*. H5.3.1 measured it instead of assuming it (§3), and the
assumption is false. So R1b was still open on Windows, and the reviewer's
`NOT_READY` was not answered by `d3267bf` — it was narrowed.

## 2. Re-audit of the final code (each item the brief requires)

| # | Area | H5.3 state at `b85355b` | H5.3.1 state |
| --- | --- | --- | --- |
| 1 | Fixture root creation | `Path.mkdir()`, then `chmod(0o700)` **on POSIX only**. On Windows the inherited parent DACL applied for the whole run | `private_root.create_owner_only_dir()`: `CreateDirectoryW` with an explicit owner-only **protected** descriptor (Windows) / `os.mkdir(0o700)` (POSIX), then read back and verified |
| 2 | Root reuse | Marker + run id + canonical path must match; hazards scanned first | Unchanged, **plus** the reused root must now pass the same DACL/mode/owner verification. A root that is not provably private is refused (fail closed) |
| 3 | Ownership marker | `goallatch-fixture-owner.json`: schema, run id, canonical root, timestamp, pid | Unchanged. The marker is still not authority — it only permits reuse of a root that independently proves private |
| 4 | Canonical path | `resolve()` for base, root and marker; the root must stay inside the resolved base | Unchanged |
| 5 | Symlink / junction / reparse point | `_is_reparse_point` (`S_ISLNK` + `FILE_ATTRIBUTE_REPARSE_POINT`), every segment from base to root, unbounded recursive scan before the marker is read | Unchanged |
| 6 | Hard link | Any regular file with `st_nlink > 1` inside a reused root is a hazard; the digest marks files `MULTIPLY_LINKED` instead of reading them | Unchanged |
| 7 | TOCTOU | Scan and write are separate operations; documented as a residual, classified wholesale as C3/R-1 | Re-classified by attacker (§4). With the root now owner-only, **another OS user cannot create the racing entry at all**; what remains is genuinely the same-user (A1) and administrator (A3) boundary |
| 8 | Windows ACL | Not checked | Full DACL read (`GetNamedSecurityInfoW` + `GetAce`); write ACEs outside `{owner, SYSTEM, Administrators, OWNER RIGHTS}` refuse the root; the owner must be the running user; a root's write ACEs must carry `OI\|CI` so children inherit them; `NULL`/absent DACL, object/callback ACEs and unmodelled ACE types are refusals. `verify_private_tree` then walks every entry |
| 9 | POSIX permission | Mode only (`& 0o077`) | Mode **and** owner uid; creation is atomic (`mkdir(0o700)`, not `mkdir()`+`chmod()`); directories require owner `rwx`, regular files only "no group/world bits" |
| 10 | Workspace / state separation | `root/state` and `root/repo`, separate session record | Unchanged; both are now verified to carry the owner-only DACL by inheritance |
| 11 | Model prompt / evidence record | `attempts/<n>/logs/step-*.txt`, `evidence.json`, prompt bytes, redaction | Unchanged |
| 12 | Test-grant isolation | `test-grant` compiled only under the non-default `test-grant-fixture` feature; issuance refuses an unmarked state root; `resolve_grant` independently refuses a `TEST_FIXTURE_V1` grant whose fixture id does not match a marker in the running state root | Unchanged — audited and confirmed, see §6 |
| 13 | Writes outside the fixture | "Create only, never delete": no deletion primitive exists in the driver (source-grep test); `_assert_disposable_base` refuses a relative base, the home directory, a drive root, a reparse-pointed base, and any base inside a Git work tree | Same rules, applied **before** anything is created: a refused base is no longer `mkdir`-ed first, only the last missing component is created, and a base whose other principals can rename/delete a child is refused |

## 3. Measured on the campaign host (2026-11-14, Windows 11, not elevated)

### 3.1 The user's `%TEMP%` is not private

```
PS> (Get-Acl (Get-Item $env:TEMP)).Sddl
O:S-1-5-21-189167550-3326330388-2875871742-1001G:S-1-5-21-...-513D:PAI
 (A;OICI;FA;;;SY)
 (A;OICI;FA;;;BA)
 (A;OICI;0x1301bf;;;S-1-5-21-2557087744-1579548355-2059345457-817895583)
 (A;OICI;FA;;;S-1-5-21-189167550-3326330388-2875871742-1001)   <- the owner (Yuta)
 (A;OICI;0x1301bf;;;S-1-5-21-189167550-3326330388-2875871742-1002)
 (A;OICI;0x1301bf;;;S-1-5-21-189167550-3326330388-2875871742-1008) <- CodexSandboxUsers
 (A;OICI;0x1301bf;;;S-1-5-21-1760078318-652840035-538058142-3187809328)
```

`FA` is `FILE_ALL_ACCESS`, `0x1301bf` is **`Modify`**. `Get-LocalUser` on the same
host lists `CodexSandboxOffline` and `CodexSandboxOnline` as enabled accounts
(the same accounts `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md` already records
for C3). So on this machine a plain directory under `%TEMP%` is writable by
another local principal — the A2 attacker C3 says must be stopped for Broker
assets.

### 3.2 What the two creation paths actually produce

Measured with the read-back check that now ships
(`private_root.trustee_report`, ACE↔SID and mask from `GetNamedSecurityInfoW`):

| Object | ACEs |
| --- | --- |
| plain `os.makedirs(dir)` under `%TEMP%` (**what H5.3 did**) | 7 ACEs, of which **4 non-owner writers at `Modify`** (RID 1002, RID 1008 = `CodexSandboxUsers`, an app-package SID, a Microsoft-account SID) |
| `os.mkdir(dir, 0o700)` under `%TEMP%` | 3 ACEs: `SYSTEM`, `Administrators`, `OWNER RIGHTS`; DACL protected |
| `os.mkdir(dir)` **then** `os.chmod(dir, 0o700)` | **unchanged 7 ACEs** — `chmod` on Windows only toggles the read-only attribute, it does not touch the DACL |
| root created by H5.3.1 | 3 ACEs: `SYSTEM`, `Administrators`, **the owner's SID**; DACL protected |
| `state/`, `repo/`, marker created inside it | inherited from the root: the same three |

### 3.3 Negative controls actually exercised

| Probe | Result |
| --- | --- |
| `SetNamedSecurityInfoW(..., DACL=nullptr)` on a throwaway directory, then `assert_private_root` | **refused**: *"has a NULL DACL, which grants everyone full access"* |
| `icacls <created root> /grant *S-1-1-0:(OI)(CI)(M)`, then reuse the root | **refused**: *"is writable by S-1-1-0 (mask 0x1301bf) … so it is refused"* (this runs in the test suite, `test_reusing_a_root_that_became_writable_by_others_is_refused`) |
| a plain directory on the shared temporary root | **refused** by the same check (positive control that the check is not vacuous) |
| `icacls <root> /grant:r <owner>:(F)` (write ACE without `OI\|CI`), then `assert_private_root` | **refused**: *"grants write … without object+container inheritance … refused"* |
| `icacls <root>/state/sessions /grant *S-1-1-0:(OI)(CI)(M)`, then `verify_private_tree` on an otherwise valid root | **refused** on the child, before reuse |
| `icacls <base> /grant *S-1-1-0:(OI)(CI)(F)` (a parent that can move a child aside) | **refused** before anything is created under it |
| `icacls <base> /grant *S-1-1-0:(OI)(CI)(M)` (write but no `FILE_DELETE_CHILD`) | **accepted** — `Modify` cannot rename or delete a protected child, which is why the default `%TEMP%` remains usable |

## 4. TOCTOU is not one problem: attacker classification

The H5.3 record collapsed every race into "C3/R-1". That was too coarse. Per the
brief, the classes are separated:

| Attacker | Can it win the scan→write race in H5.3? | In H5.3.1? | Where it belongs |
| --- | --- | --- | --- |
| Another OS user (A2), restricted | **Yes** — inherited `Modify` on a `%TEMP%` root let them create/replace entries inside a fixture root | **No** for entries *inside* the root: it carries no ACE for them (verified by read-back). For the root's *name* and **every directory on the path to it** (round 7): refused when an untrusted trustee holds a rename/delete bypass, when the directory is foreign-owned (an owner can always rewrite the DACL or `chmod`), or when the ACL uses an ACE type this check cannot evaluate | Was an H5.3 **harness defect**, closed in three steps: root DACL (round 6), base entry (round 6), whole ancestor chain + foreign-owner rule + ACE-type refusal (round 7) |
| Another OS user in a directory the operator deliberately shared | Yes | No: such a root is refused on reuse; a new root never inherits the shared ACL | Closed by refusal |
| Same OS user, another process (A1) | Yes | **Yes** | C3 / R-1 — genuinely unresolved, and unfixable inside one account |
| Elevated / `SYSTEM` (A3) | Yes | **Yes** | Out of scope by design (an administrator *is* the Owner) — `C3_0` §6.1 |
| Operator error (wrong `--work-dir`) | No — the base checks already refused home/drive/Git roots and the driver has no deletion primitive | No | Closed in H5.3-0 |

So the honest statement is: **the cross-user part of R1b is fixed in two places
and provable** — the root's contents (owner-only DACL, verified) and the root's
*name* (a base whose other principals can delete or rename children is refused),
**and the same-user part is C3/R-1 and remains open.**

### 4.0 Scope of the A2 claim, stated precisely

The A2 claim is: *for a fixture root created by this driver, an unrelated local
account has no write access to its contents and no ability to replace any name on
the path to it*, **as measured at creation time**. It is a snapshot plus a
refusal, not a kernel-enforced continuation:

* the ACLs and modes of the whole path are read and refused if any of them allows
  a stranger to write, re-ACL, or rename a component;
* nothing re-checks them later, so an attacker who gains such a capability *after*
  creation is not stopped by this harness. Gaining it requires write access to an
  ancestor the driver has just refused, or administrator rights (A3);
* writing *through* a path that is checked and then used remains a
  path-based-operation race for the same user (A1) — that is C3/R-1, and it is not
  fixed here.

### 4.1 What round 6 of the independent review changed here

The first fix only protected the contents. The round-6 reviewer showed that this
was not enough, in three ways that are now closed:

| Round-6 finding | Severity | Disposition |
| --- | --- | --- |
| A writable parent still permits cross-user replacement of the **root entry** | High | `private_root.assert_parent_cannot_reenter`: a base is refused when an untrusted trustee holds `FILE_DELETE_CHILD`/`WRITE_DAC`/`WRITE_OWNER`/`GENERIC_ALL` on it (Windows) or when it is group/other-writable and not sticky (POSIX). `Modify` on the parent is *not* enough to move a protected child, which is why the default `%TEMP%` stays usable |
| Accepted roots need not **propagate** their protection to children | High | The root check now requires every write ACE to carry `OI|CI` (Windows), and `verify_private_tree` walks and verifies **every** entry after creation and on reuse instead of a hand-picked sample |
| POSIX creation **failed** on ordinary seeded files | High | `_posix_assert` required `0o700` of regular files, so the marker (0644/0600) could never pass. Files are now checked only for owner and for no group/world bits; directories keep the `0o700` requirement. This was a real regression introduced by the first fix |
| Base validation created directories before refusing, and resolved before checking links | Medium | `_refuse_unsafe_base_path` runs on the supplied path first; only the last missing component is created, and a missing intermediate directory is refused |
| The checkpoint claimed a per-call no-write proof that did not exist | Medium | The digest is now taken around **each** model call and recorded per call; the evidence still says "digest comparison", never "OS-enforced read-only isolation" |
| The closure record cited a review section that did not exist yet | Low | `H5_3_EVIDENCE.md` §10 now carries round 6 verbatim with its dispositions |

### 4.2 What round 7 changed

Round 7 reviewed the result of round 6 and refused it again. All five findings
were correct; two were defects introduced by the round-6 fix itself.

| Round-7 finding | Severity | Disposition |
| --- | --- | --- |
| POSIX creation raised `TypeError` (`_posix_assert` gained a parameter that `create_owner_only_dir` did not pass), and seeded files written with the umask (`0644`) were then refused by the tree check | High | `create_owner_only_dir` passes `expect_directory=True`; every fixture file is written through `_write_private_text`, which sets `0600` on POSIX (the file belongs to this harness, inside its own root) |
| A **foreign-owned** base was treated as trusted: a Windows owner can always rewrite the DACL, and a sticky POSIX directory still lets its owner remove an entry | High | Only the running user or a trusted principal (`SYSTEM`, `Administrators`, `TrustedInstaller`, POSIX `root`) may own a component of the path; a foreign owner is refused |
| A replaceable **ancestor** could redirect an accepted base (the round-6 check only looked at the base itself) | High | `assert_path_cannot_reenter` checks every directory from the base up to the filesystem root |
| The Windows parent check skipped object and callback ACE types | High | Any ACE that is neither plain allow nor plain deny is a refusal there too |
| An **empty** Windows DACL was accepted as a private root although it is unusable | Medium | A root must contain a write ACE for its owner, otherwise it is refused as unusable rather than declared private |

## 5. What H5.3.1 changed (code)

| File | Change |
| --- | --- |
| `scripts/h5-d/private_root.py` | **New.** Owner-only root creation and verification (Windows DACL via `advapi32`, POSIX mode), `trustee_report` for evidence, and a fail-closed policy for anything it cannot prove |
| `scripts/h5-d/real_model_scenario_a.py` | `_assert_private_fixture_root` delegates to `private_root.assert_private_root` on **both** platforms (the Windows early-return is gone); new roots are created with `create_owner_only_dir`; the marker, `state/`, `state/sessions/`, `repo/` and the seeded files are verified after creation |
| `scripts/h5-d/test_fixture_safety.py` | +5 tests (30 total): owner-only ACE set on the root, the temporary directory is measurably shared while the root does not inherit it, an `Everyone`-granted root is refused on reuse, a group-writable root is refused on POSIX, an owner-only root is accepted and reused |
| `docs/campaign/H5_3_0_DRIVER_SAFETY.md` | §2/§4/§6 corrected (the Windows claim was wrong), new §7 with the mechanism and the measurement |
| `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md` | Host-fact note: A2 write access on `%TEMP%` is measured, not assumed. The C3 verdict is unchanged |

Deliberately **not** done, per the brief's warning about inventing new Windows
permission machinery: no ACL is modified outside the root the harness just
created, no global ACE, user, service or policy is touched, and no attempt is
made to defend the root against the account that owns it.

## 6. Test-grant isolation (audited, unchanged)

| Claim | Evidence |
| --- | --- |
| No production build can mint a `TEST_FIXTURE_V1` grant | `Cargo.toml`: `default = []`, `test-grant-fixture = []`; `src/main.rs` gates the `test-grant` subcommand with `#[cfg(feature = "test-grant-fixture")]` |
| A feature-enabled build still refuses a real state root | `issue_test_fixture_grant` requires the state root to carry `test-grant-fixture.json` and to be marked disposable (`FixtureGrantError::NotAFixtureRoot`) |
| A fixture grant copied into a real state root is inert | `config::test_grant_fixture_id()` is read on the grant-resolution path, and `resolve_grant` refuses `TEST_FIXTURE_V1` when the fixture id does not match a marker in the running state root |

## 7. Local verification

| Check | Result |
| --- | --- |
| `python scripts/h5-d/test_fixture_safety.py` | **30 tests, OK** (1 platform skip: the POSIX-mode test on Windows) |
| `cargo fmt --all -- --check` | PASS |
| `cargo check --locked --all-targets` | see §9 |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | see §9 |
| `cargo test --locked` | see §9 |
| `cargo test --locked --features test-grant-fixture` | see §9 |

No Rust source changed in this phase, so the Rust results are a regression check
of the fused HEAD, not a new-code check.

## 8. Not verified (stated, not implied)

* Another *real* OS user was not logged in to attempt the race. The claim that
  they cannot create an entry inside the root is derived from the measured DACL
  plus the kernel's access-check semantics, and from the negative control that
  the check refuses an `Everyone:(M)` root. It is not an executed cross-user
  attack.
* Windows junction traversal and concurrent filesystem replacement remain
  reasoned, not executed (carried forward from H5.3).
* The `OWNER RIGHTS` (`S-1-3-4`) acceptance path is reasoned: an ACE for that SID
  is only evaluated for the object's owner, and the owner is separately proven to
  be the running user. It was observed on this host (`os.mkdir(path, 0o700)`
  produces it) but not attacked.
* Domain/`gMSA`-style principals, and hosts where `%TEMP%` is redirected to a
  network share, were not exercised.

## 9. Independent review of this HEAD

Recorded in `H5_3_EVIDENCE.md` §10 (round 6) with the model id, reasoning
setting, reviewed SHA, scope, findings by severity and the reviewer's verdict.
The implementer's opinion of its own work is not a verdict.
