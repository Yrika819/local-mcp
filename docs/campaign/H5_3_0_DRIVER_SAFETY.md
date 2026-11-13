# H5.3-0 — Real-model driver safety

Campaign: GoalLatch H5.3 — External Backend Final Validation
Branch: `feat/h5-3-external-backend-closure-v1` (from PR #56 HEAD `0dcab4a`)
Base SHA at H5.3 start: `0dcab4a109b73d88b7f74871bde70d184e8f147d`

This document covers only the driver under `scripts/h5-d/`. It describes what the
driver is allowed to do to the filesystem, why the previous version was unsafe,
and which tests now hold the property.

---

## 1. The defect

The H5.2 driver created its fixture like this:

```python
def make_fixture(work_dir: Path):
    root = work_dir.resolve()
    if root.exists():
        shutil.rmtree(root)
```

`work_dir` came from `--work-dir`, whose default was the repository-relative
`tmp/h5d/scenario-a`. That is an unbounded recursive delete of an
operator-supplied path. A typo — `--work-dir .`, `--work-dir ..`,
`--work-dir %USERPROFILE%` — deletes the working tree or the user profile with
no confirmation, no ownership check and no recovery. It also destroyed the
previous run's `logs/` and `evidence.json`, which is why the H5.2-C run 1
evidence no longer exists.

Because a real-model campaign is exactly the situation where an operator types
an unusual path, this was treated as a release blocker rather than a nuisance.

## 2. The replacement design

`make_fixture(work_dir=None, run_id=None)` now **creates, and never deletes**.

| Requirement | Implementation |
| --- | --- |
| Unique new root under an **operator-chosen private** base | `base = work_dir` (no implicit default: see §7.3), `root = base / f"goallatch-h5d-{run_id}"`, `run_id` = UTC timestamp + 8 hex characters unless supplied |
| Never unconditionally delete an arbitrary path | The driver contains no deletion primitive at all; `test_no_deletion_helper_exists_in_the_driver` greps the source for `shutil.rmtree`, `os.rmdir`, `os.removedirs` and `unlink(` |
| Reuse of an existing directory only when owned | `goallatch-fixture-owner.json` records the schema, run id, canonical root, creation time and owning pid; `_claim_existing_root` requires the schema, an exact run-id match **and** `realpath(root) == marker.canonical_root`. An unmarked or foreign root is refused and left untouched |
| Reject symlinks, Windows junctions and reparse points | `_is_reparse_point` checks `S_ISLNK` and `FILE_ATTRIBUTE_REPARSE_POINT`; `_reparse_components` walks every existing segment from the base to the root; the resolved root must still be *inside* the resolved base |
| State root and workspace separated | `root/state` and `root/repo`, never the same directory |
| Never target the primary repository, a worktree, or the user home | `_assert_disposable_base` refuses a relative base, the home directory, a drive root, any base reached through a reparse point, and any base with a `.git` entry in any ancestor |
| Evidence per run | `attempts/<n>/evidence.json` and `attempts/<n>/logs/`, with `<n>` monotonically allocated per root |
| Previous logs and evidence are never overwritten | Because `attempts/<n>` is freshly created per invocation and no file is ever truncated |
| Only the owner can write inside the root | `private_root.create_owner_only_dir` creates the root **with** an owner-only descriptor (`SYSTEM`, `Administrators`, the creating user) on Windows and `0o700` on POSIX, then reads the DACL back and verifies it; `verify_private_tree` verifies every entry, after creation and on reuse. The per-entry check proves the property, not the inheritance mechanism. See §7.1 and `docs/campaign/H5_3_1_FINAL_CLOSURE.md` |
| The root's *name* cannot be swapped | Every directory from the base up to the filesystem root is refused when an untrusted trustee can delete or rename a child of it (`FILE_DELETE_CHILD`/`WRITE_DAC`/`WRITE_OWNER`, or an ACE type this check cannot evaluate, on Windows; group/other-writable and not sticky on POSIX), when the directory is owned by anyone other than the running user or a trusted principal, and when an ACE type is not a plain allow/deny |

### 2.1 Why "never delete" rather than "delete only what I own"

The campaign brief prefers generating a throwaway root and keeping it over any
deletion path. A delete path has to be correct forever, including on a machine
where a path is a junction, a network share, or a case-insensitive alias of
something important. A create-only driver cannot be wrong about that, and the
cost — a few megabytes under `%TEMP%` — is negligible next to a lost worktree.

## 3. Model-invocation hardening

| Concern | Behaviour |
| --- | --- |
| Answer extraction | `_first_json_object` scans for the first **balanced** `{...}` with string and escape awareness. The previous greedy `re.search(r"\{.*\}", DOTALL)` would span two objects or swallow trailing prose |
| Empty output | `MODEL_OUTPUT_EMPTY` |
| No JSON object | `MODEL_OUTPUT_NO_JSON` |
| Unparseable object | `MODEL_OUTPUT_INVALID_JSON` |
| Non-object answer | `MODEL_OUTPUT_NOT_OBJECT` |
| CLI missing | `MODEL_CLI_MISSING` |
| Non-zero exit | `MODEL_EXIT_NONZERO`, with the exit code and a redacted stderr tail |
| Timeout | `MODEL_TIMEOUT`, with the elapsed seconds |

Every classification is a `ModelInvocationError`, so a failure is never silently
converted into a passing run, and nothing is retried automatically: a retry is a
deliberate driver decision with a recorded reason.

| Concern | Behaviour |
| --- | --- |
| Log redaction | `redact()` masks `Authorization: Bearer …`, GitHub tokens, `sk-…` keys and `key=value`-shaped secrets before any prompt, stdout or stderr reaches disk |
| Contract forwarding | The prompt carries `schema`, `conditions` **and** `notes` (H5.3-A3). `conditions_forwarded` and `notes_forwarded` are recorded per call |
| Prompt size | Recorded as `prompt_bytes` per call; the Replanner request is passed on stdin because a Windows command line is limited to ~32 KiB |

## 4. Tests that hold the property

`scripts/h5-d/test_fixture_safety.py` — 40 tests, no model, no binary, runs in CI
(the `quality` job gained a `Run real-model driver safety self-tests` step):

* unique root per run, `state`/`repo` separated, marker written and correct;
* the documented fixture files exist, including `data/inventory.txt`;
* relative bases, unsafe run ids, foreign markers and unowned directories are
  refused, and an unowned directory's contents survive byte-for-byte;
* the driver source contains no deletion primitive;
* reusing an owned run creates `attempts/2` and leaves `attempts/1` intact;
* a symlinked root is refused and the link target is untouched;
* a base inside a Git work tree and the user home are refused;
* the answer parser handles nested braces, braces inside strings, escaped quotes
  and two objects in one answer, and classifies every failure mode;
* redaction masks credential shapes and leaves ordinary text alone;
* `notes` reach the prompt, and an empty note list is stated explicitly;
* H5.3.1: a created root names only the owner, `SYSTEM` and `Administrators` in
  its write ACEs and does not inherit any ACE of the OS temporary directory;
  the same temporary directory is *measurably* shared on the campaign host, so
  the check is not vacuous;
* H5.3.1: a root that later becomes writable by `Everyone` (Windows) or
  group/world-writable (POSIX) is refused when it is reused, and a *child* whose
  DACL diverges from the root's is refused by the whole-tree verification;
* H5.3.1: a root whose write ACEs lack `OI\|CI` is refused, and so is a base whose
  other principals can rename or delete a child of it.

Windows job `test (windows-x64)` runs the Rust suite that exercises the same
publication and recovery code; the Python test above runs on the Linux
`quality` job, so the driver's safety properties are checked on both.

## 5. What this does not cover

* The driver is test tooling. It does not ship, and `test-grant` remains
  compiled only under the non-default `test-grant-fixture` feature.
* `_assert_disposable_base` refuses a base reached through a reparse point. On a
  host whose `%TEMP%` is redirected by a junction, the operator must pass an
  explicit `--work-dir` on a plain directory. Failing closed is intentional.
* Retained fixture roots are not garbage-collected. An operator removes them
  deliberately; the driver never will.
* A root created by an *older* driver version inherits the temporary
  directory's DACL, so it is no longer reusable: reuse refuses it (fail closed)
  and the operator starts a new run id. Nothing is deleted.



## 6. Residual: harness TOCTOU (documented, not defended)

The independent review noted that the hazard scan and the fixture writes are
separate filesystem operations, so a process that can create entries inside an
*owned* fixture root between the two could still win the race. This is recorded
rather than defended, for a specific reason:

* H5.3 claimed that the root was owner-only, but that was only true on POSIX:
  `_assert_private_fixture_root` returned early on Windows and `Path.mkdir()`
  inherited the parent DACL. H5.3.1 corrected this — the root is now created
  with an owner-only descriptor on both platforms and verified by reading the
  DACL/mode back (§7.1) — so **another OS user can no longer create entries in a
  fixture root**, and the race is out of reach for that attacker. What remains is
  a process running as the *same* OS user (or as an administrator), which is the
  C3/R-1 boundary this program does not solve; the fixture-root DACL is not a
  defence against the account that owns it.
* what the scan does guarantee is the property that mattered: an operator cannot
  point the harness at a root that *already* redirects somewhere else. The scan
  runs before the ownership marker is read, so a marker reachable through a hard
  link cannot impersonate ownership either.

Making the check atomic would require opening every file with
`O_NOFOLLOW`-equivalent semantics and keeping the descriptors, which is not
portable to Windows for directories. Recording the limit is the honest option.

## 7. H5.3.1 — the root is private on Windows too

### 7.1 What changed

| Before (H5.3) | After (H5.3.1) |
| --- | --- |
| `root.mkdir()` — inherits the parent DACL on Windows | `private_root.create_owner_only_dir()` — `CreateDirectoryW` with an owner-only, *protected* security descriptor, so the descriptor is in place at creation and there is no hardening window |
| `_assert_private_fixture_root` returned immediately on Windows | The DACL is read back (`GetNamedSecurityInfoW` + `GetAce`), and any write ACE for a trustee outside `{owner, SYSTEM, Administrators, OWNER RIGHTS}` refuses the root |
| Ownership of the root was never checked | The descriptor's owner must be the running user, otherwise the root is refused |
| Only the root itself was checked | The marker, `state/`, `state/sessions/`, `repo/` and the seeded files are verified after creation, so inheritance is proven to have carried the descriptor down |
| Verification could not be exercised negatively | `test_fixture_safety.py` gains a positive control (the OS temporary directory *is* shared on the campaign host) and a negative control (`icacls … *S-1-1-0:(M)` on a created root is refused on reuse) |

Everything is fail-closed: an unreadable descriptor, a `NULL` DACL, a
non-protected root DACL, an object/callback ACE, an unmodelled ACE type, an
unexpected trustee or a different owner is a *refusal*, never a warning. No ACL
outside the root the harness just created is modified.

### 7.2 Measured on the campaign host (2026-11-14)

```
%TEMP%  D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)
        (A;OICI;0x1301bf;;;S-1-5-21-...-1002)
        (A;OICI;0x1301bf;;;S-1-5-21-...-1008) -> <host>\CodexSandboxUsers
        (A;OICI;0x1301bf;;;S-1-5-21-...)
        (A;OICI;FA;;;S-1-5-21-...-1001)       -> the owner

plain mkdir under %TEMP%   7 ACEs, four non-owner write ACEs (Modify)
root created by H5.3.1     3 ACEs: SYSTEM, Administrators, the owner; protected
state/, marker             inherited from the root: the same three
```

So the H5.3 assumption "`%TEMP%` is already per-user" was false on real
hardware: a plain directory there is writable by another local account and by a
sandbox group. The evidence is reproduced by
`test_the_os_temporary_directory_is_shared_and_the_root_does_not_inherit_it`.

### 7.3 The default base had to change as well

Protecting the root is not enough while the *name* of the base can be moved
(round 6/7/8 review). Windows authorises a rename or delete of a component
through `DELETE` on that component, and `Modify` (`0x1301bf`) contains `DELETE` —
so on this host another local account can rename the user's `%TEMP%` itself and
substitute a junction. The consequence is deliberate and fail-closed:

* `%TEMP%` is **no longer an implicit default** (`make_fixture` refuses a call
  with no base and says why);
* a base is accepted only when **every** directory on the path to it is owned by
  the running user or a trusted principal, and no untrusted trustee can rename or
  delete a component (`DELETE`, `FILE_DELETE_CHILD`, `WRITE_DAC`, `WRITE_OWNER`,
  `GENERIC_ALL`, or any ACE type this check cannot evaluate);
* on POSIX the equivalent rule is "not group/other-writable unless sticky, and
  owned by the user or root", so a sticky root-owned `/tmp` is still accepted;
* inside the home tree a **dedicated directory** is now allowed (that is the only
  provably private place on this host), while the home root and the usual shell
  folders (`Documents`, `Desktop`, `Downloads`, …) are still refused.

The operator therefore passes `--work-dir` explicitly, for example
`--work-dir ~/goallatch-h5d-fixtures`.
