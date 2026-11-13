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
| Unique new root under the OS temp directory | `base = work_dir or tempfile.gettempdir()`, `root = base / f"goallatch-h5d-{run_id}"`, `run_id` = UTC timestamp + 8 hex characters unless supplied |
| Never unconditionally delete an arbitrary path | The driver contains no deletion primitive at all; `test_no_deletion_helper_exists_in_the_driver` greps the source for `shutil.rmtree`, `os.rmdir`, `os.removedirs` and `unlink(` |
| Reuse of an existing directory only when owned | `goallatch-fixture-owner.json` records the schema, run id, canonical root, creation time and owning pid; `_claim_existing_root` requires the schema, an exact run-id match **and** `realpath(root) == marker.canonical_root`. An unmarked or foreign root is refused and left untouched |
| Reject symlinks, Windows junctions and reparse points | `_is_reparse_point` checks `S_ISLNK` and `FILE_ATTRIBUTE_REPARSE_POINT`; `_reparse_components` walks every existing segment from the base to the root; the resolved root must still be *inside* the resolved base |
| State root and workspace separated | `root/state` and `root/repo`, never the same directory |
| Never target the primary repository, a worktree, or the user home | `_assert_disposable_base` refuses a relative base, the home directory, a drive root, any base reached through a reparse point, and any base with a `.git` entry in any ancestor |
| Evidence per run | `attempts/<n>/evidence.json` and `attempts/<n>/logs/`, with `<n>` monotonically allocated per root |
| Previous logs and evidence are never overwritten | Because `attempts/<n>` is freshly created per invocation and no file is ever truncated |

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

`scripts/h5-d/test_fixture_safety.py` — 22 tests, no model, no binary, runs in CI
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
* `notes` reach the prompt, and an empty note list is stated explicitly.

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

## 6. Residual: harness TOCTOU (documented, not defended)

The independent review noted that the hazard scan and the fixture writes are
separate filesystem operations, so a process that can create entries inside an
*owned* fixture root between the two could still win the race. This is recorded
rather than defended, for a specific reason:

* the fixture root is already owned by the invoking user and lives under the OS
  temporary directory; a process that can create entries there is running as the
  same OS user and already has the operator's authority. That is exactly the
  C3/R-1 same-user boundary that this program does not solve;
* what the scan does guarantee is the property that mattered: an operator cannot
  point the harness at a root that *already* redirects somewhere else. The scan
  runs before the ownership marker is read, so a marker reachable through a hard
  link cannot impersonate ownership either.

Making the check atomic would require opening every file with
`O_NOFOLLOW`-equivalent semantics and keeping the descriptors, which is not
portable to Windows for directories. Recording the limit is the honest option.
