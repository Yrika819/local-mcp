#!/usr/bin/env python3
"""H5.3-0 — the real-model driver must not be able to destroy user data.

These tests exercise `make_fixture()` and the model-output classifier in
`scripts/h5-d/real_model_scenario_a.py` **without** invoking a model and without
a `local-mcp` binary, so they are safe to run in CI.

The property under test is the H5.3-0 gate: a run of the real-model driver can
only ever create a new, uniquely owned, disposable fixture root. It never
deletes an arbitrary path, never reuses an unowned directory, never overwrites a
previous run's logs or evidence, and never follows a symlink or Windows junction
out of its own root.

Run:
    python scripts/h5-d/test_fixture_safety.py
"""

from __future__ import annotations

import json
import os
import stat
import sys
import tempfile
import unittest
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import real_model_scenario_a as driver  # noqa: E402


#: One private root for every fixture test. The OS temporary directory is refused
#: as a base (another local account can rename or delete it on a shared machine),
#: so the tests create their bases under the user profile, exactly as an operator
#: now has to. Nothing is deleted - the same policy the driver follows.
TEST_FIXTURE_ROOT = Path.home() / ".goallatch-h5d-test-fixtures"


class DisposableBase(unittest.TestCase):
    def setUp(self) -> None:
        # Round 9 (Medium): the tests must not change the mode of a path that
        # might already exist, and must not follow a link. The shared root is
        # created once, then *verified*; if it is not private the test fails
        # instead of repairing something that is not this suite's to repair.
        root = TEST_FIXTURE_ROOT
        if root.is_symlink():
            self.fail(f"{root} is a symbolic link; refusing to use it")
        if not root.exists():
            root.mkdir(mode=0o700)
        info = root.lstat()
        if os.name == "nt":
            # A junction/reparse point or a foreign ACL on a pre-existing shared
            # test root would redirect or expose everything created beneath it,
            # and the driver's own checks only cover the base it is given.
            import private_root as pr

            if getattr(info, "st_file_attributes", 0) & 0x400:
                self.fail(f"{root} is a reparse point; refusing to use it")
            try:
                pr.assert_private_root(root)
            except pr.PrivateRootError as error:
                self.fail(f"{root} is not private: {error}")
        if os.name != "nt":
            if info.st_uid != os.geteuid():
                self.fail(f"{root} is owned by uid {info.st_uid}, not by this user")
            if stat.S_IMODE(info.st_mode) & 0o077:
                self.fail(
                    f"{root} is group/world accessible; refusing to change an "
                    "existing directory's mode"
                )
        self.base = root / (self._testMethodName + "-" + uuid.uuid4().hex[:8])
        self.base.mkdir()

    def tearDown(self) -> None:
        pass


class FixtureCreationTests(DisposableBase):
    def test_a_run_creates_a_new_unique_root_with_separated_state_and_workspace(
        self,
    ) -> None:
        first = driver.make_fixture(self.base)
        second = driver.make_fixture(self.base)

        self.assertNotEqual(first.root, second.root)
        self.assertTrue(first.state.is_dir())
        self.assertTrue(first.workspace.is_dir())
        self.assertEqual(first.state.parent, first.root)
        self.assertEqual(first.workspace.parent, first.root)
        self.assertEqual(first.state, first.root / "state")
        self.assertEqual(first.workspace, first.root / "repo")
        self.assertFalse(first.state == first.workspace)

        marker = json.loads(
            (first.root / driver.FIXTURE_MARKER_NAME).read_text(encoding="utf-8")
        )
        self.assertEqual(marker["schema"], driver.FIXTURE_MARKER_SCHEMA)
        self.assertEqual(marker["run_id"], first.run_id)
        self.assertEqual(marker["canonical_root"], str(first.root.resolve()))

    def test_the_fixture_contains_the_documented_workspace_files(self) -> None:
        fixture = driver.make_fixture(self.base)
        self.assertTrue((fixture.workspace / "src" / "main.rs").is_file())
        self.assertTrue((fixture.workspace / "src" / "helper.rs").is_file())
        inventory = fixture.workspace / "data" / "inventory.txt"
        self.assertTrue(inventory.is_file())
        self.assertIn("widgets=42", inventory.read_text(encoding="utf-8"))

    def test_a_relative_base_is_refused(self) -> None:
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(Path("tmp/relative-fixture"))

    def test_an_unsafe_run_id_is_refused(self) -> None:
        for run_id in ("../escape", "a/b", "", "x" * 200):
            with self.assertRaises(driver.FixtureSafetyError):
                driver.make_fixture(self.base, run_id)


class NoDeletionTests(DisposableBase):
    def test_an_existing_unowned_directory_is_refused_and_left_intact(self) -> None:
        root = self.base / "goallatch-h5d-owned-run"
        root.mkdir()
        sentinel = root / "operator-data.txt"
        sentinel.write_text("must survive\n", encoding="utf-8")

        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "owned-run")

        self.assertTrue(sentinel.is_file())
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "must survive\n")
        self.assertFalse((root / "state").exists())

    def test_a_foreign_marker_is_refused(self) -> None:
        root = self.base / "goallatch-h5d-run-x"
        root.mkdir()
        (root / driver.FIXTURE_MARKER_NAME).write_text(
            json.dumps(
                {
                    "schema": driver.FIXTURE_MARKER_SCHEMA,
                    "run_id": "someone-else",
                    "canonical_root": str(root.resolve()),
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "run-x")

    def test_no_deletion_helper_exists_in_the_driver(self) -> None:
        source = Path(driver.__file__).read_text(encoding="utf-8")
        for forbidden in ("shutil.rmtree", "os.removedirs", "os.rmdir", "unlink("):
            self.assertNotIn(
                forbidden,
                source,
                f"the driver must not be able to delete: found {forbidden}",
            )


class ReuseTests(DisposableBase):
    def test_reusing_an_owned_run_never_overwrites_the_previous_attempt(self) -> None:
        first = driver.make_fixture(self.base, "reused-run")
        first.logs.mkdir(parents=True, exist_ok=True)
        first.evidence_path.write_text('{"attempt": 1}\n', encoding="utf-8")
        (first.logs / "step-0-prompt.txt").write_text("first", encoding="utf-8")
        # The first run holds the exclusive lease; reuse is what happens after it
        # is finished, so the lease is released here.
        first.release()

        second = driver.make_fixture(self.base, "reused-run")

        self.assertEqual(first.root, second.root)
        self.assertNotEqual(first.attempt_dir, second.attempt_dir)
        self.assertEqual(
            first.evidence_path.read_text(encoding="utf-8"), '{"attempt": 1}\n'
        )
        self.assertEqual(
            (first.logs / "step-0-prompt.txt").read_text(encoding="utf-8"), "first"
        )
        self.assertFalse(second.evidence_path.exists())


class ReparsePointTests(DisposableBase):
    def _make_link(self, target: Path, link: Path) -> bool:
        try:
            if os.name == "nt":
                os.symlink(target, link, target_is_directory=True)
            else:
                link.symlink_to(target, target_is_directory=True)
            return True
        except (OSError, NotImplementedError):
            return False

    def test_an_owned_root_containing_a_nested_symlink_is_refused(self) -> None:
        # Independent review R1 (Medium): reuse validated the root and marker but
        # not its descendants, so an owned root whose `repo` was a junction would
        # have redirected the fixture writes outside the root.
        owned = driver.make_fixture(self.base, "nested-link")
        outside = self.base / "nested-target"
        outside.mkdir()
        (outside / "keep.txt").write_text("keep\n", encoding="utf-8")
        link = owned.root / "linked"
        if not self._make_link(outside, link):
            self.skipTest("this host cannot create directory symlinks")

        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "nested-link")

        self.assertEqual(
            (outside / "keep.txt").read_text(encoding="utf-8"), "keep\n"
        )
        self.assertFalse((outside / "src" / "main.rs").exists())

    def test_a_deep_nested_symlink_is_refused_and_never_digested(self) -> None:
        # Re-review R1 (Low): a depth-limited scan accepted a link deeper than the
        # limit, and the workspace digest then read the link target's bytes.
        owned = driver.make_fixture(self.base, "deep-link")
        outside = self.base / "deep-target"
        outside.mkdir()
        (outside / "secret.txt").write_text("outside\n", encoding="utf-8")
        deep = owned.root / "repo" / "a" / "b" / "c" / "d" / "e"
        deep.mkdir(parents=True)
        link = deep / "link.txt"
        try:
            os.symlink(outside / "secret.txt", link)
        except (OSError, NotImplementedError):
            self.skipTest("this host cannot create file symlinks")

        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "deep-link")

        digest = driver.workspace_digest(owned.root / "repo")
        self.assertEqual(
            digest.get("a/b/c/d/e/link.txt"),
            "REPARSE_POINT",
            "the digest must refuse to read through a link, not hash its target",
        )

    def test_a_hard_linked_fixture_file_is_refused(self) -> None:
        # Re-review round 3 (Medium): a hard link is not a reparse point, so the
        # scan accepted it and write_text would have truncated the shared file.
        owned = driver.make_fixture(self.base, "hard-link")
        outside = self.base / "hard-target.txt"
        outside.write_text("must survive\n", encoding="utf-8")
        target = owned.root / "repo" / "src" / "main.rs"
        try:
            os.remove(target)
            os.link(outside, target)
        except (OSError, NotImplementedError, AttributeError):
            self.skipTest("this host cannot create hard links")

        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "hard-link")

        self.assertEqual(
            outside.read_text(encoding="utf-8"),
            "must survive\n",
            "reuse must not truncate a file outside the root",
        )
        digest = driver.workspace_digest(owned.root / "repo")
        self.assertEqual(digest.get("src/main.rs"), "MULTIPLY_LINKED")

    def test_a_symlinked_root_is_refused(self) -> None:
        outside = self.base / "outside"
        outside.mkdir()
        (outside / "keep.txt").write_text("keep\n", encoding="utf-8")
        root = self.base / "goallatch-h5d-linked-run"
        if not self._make_link(outside, root):
            self.skipTest("this host cannot create directory symlinks")

        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "linked-run")

        self.assertEqual(
            (outside / "keep.txt").read_text(encoding="utf-8"), "keep\n"
        )


class ProtectedBaseTests(DisposableBase):
    def test_a_base_inside_a_git_work_tree_is_refused(self) -> None:
        repo = self.base / "repo"
        repo.mkdir()
        (repo / ".git").mkdir()
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(repo / "fixtures")

    def test_the_user_home_directory_is_refused(self) -> None:
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(Path.home())


class OwnerOnlyRootTests(DisposableBase):
    """H5.3.1: the root, not just its top entry, must resist another OS user.

    The parent used here is `tempfile.TemporaryDirectory`, i.e. the real OS
    temporary directory. On the campaign host that parent grants `Modify` to
    further local principals (measured with `Get-Acl`), which is exactly why the
    root can no longer rely on inheritance.
    """

    _WRITE_ALLOWED = {"S-1-5-18", "S-1-5-32-544", "S-1-3-4"}

    def _owner_sid(self) -> str:
        import private_root as pr  # local import: only needed on Windows

        api = pr._load_windows_api()
        return pr._current_user_sid(api)

    def test_a_created_root_names_only_the_owner_and_trusted_principals(self) -> None:
        if os.name != "nt":
            self.skipTest("Windows DACL check")
        fixture = driver.make_fixture(self.base, "owner-only")
        import private_root as pr

        owner = self._owner_sid()
        allowed = self._WRITE_ALLOWED | {owner}
        for row in pr.trustee_report(fixture.root):
            if row["write_rights"]:
                self.assertIn(
                    row["trustee"],
                    allowed,
                    f"the root must not be writable by {row['trustee']} ({row['mask']})",
                )
            self.assertTrue(row["dacl_protected"], "a root must not inherit ACEs")
            self.assertEqual(row["owner"], owner)

    def test_the_os_temporary_directory_is_shared_and_the_root_does_not_inherit_it(
        self,
    ) -> None:
        # Positive control for the check above: the parent the harness defaults to
        # is measurably shared, and none of those trustees appear on the root.
        import private_root as pr

        parent = Path(tempfile.gettempdir())
        rows = pr.trustee_report(parent)
        if os.name == "nt":
            owner = self._owner_sid()
            shared = [
                row
                for row in rows
                if row.get("write_rights")
                and row["trustee"] not in self._WRITE_ALLOWED | {owner}
            ]
        else:
            shared = [rows[0]] if rows[0]["group_or_world_bits"] else []
        if not shared:
            self.skipTest("the OS temporary directory is private on this host")
        if os.name == "nt":
            # `Modify` includes DELETE on the object, so another account can rename
            # the temporary directory itself: it is refused as a base.
            with self.assertRaises(driver.FixtureSafetyError):
                driver.make_fixture(parent, "shared-temp")
        else:
            # A sticky, root-owned /tmp only lets the entry owner (and root) remove
            # an entry, so it stays an acceptable base on POSIX. The check is
            # asserted directly: creating a fixture under the shared temporary
            # directory would adopt or rewrite a directory this test did not create
            # (round 11, Medium).
            pr.assert_path_cannot_reenter(parent)

        fixture = driver.make_fixture(self.base, "shared-parent")
        if os.name != "nt":
            mode = stat.S_IMODE(fixture.root.lstat().st_mode)
            self.assertEqual(
                mode & 0o077, 0, f"the root must be owner-only, got {oct(mode)}"
            )
            return
        root_trustees = {
            row.get("trustee")
            for row in pr.trustee_report(fixture.root)
            if row.get("write_rights")
        }
        for row in shared:
            self.assertNotIn(
                row.get("trustee"),
                root_trustees,
                "the root must not inherit a write ACE from the temporary directory",
            )

    def test_a_base_whose_entries_anyone_can_replace_is_refused(self) -> None:
        # Review round 6 (High): a private root does not protect its *name*. If the
        # parent lets another principal delete or rename children (Windows
        # `FILE_DELETE_CHILD`, POSIX group/other-writable and not sticky), the root
        # can be moved aside and replaced by a link, and every later write follows
        # the link - so that base is refused before anything is created.
        parent = self.base / "reenterable-parent"
        parent.mkdir()
        self._make_root_writable_by_everyone(parent, rights="F")
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(parent, "reenter")
        self.assertFalse((parent / "goallatch-h5d-reenter").exists())

    def test_a_sticky_shared_parent_is_accepted_on_posix(self) -> None:
        # /tmp is 1777 *with the sticky bit*: another user may create entries but
        # may not rename or delete ours, so it stays a usable base.
        if os.name == "nt":
            self.skipTest("POSIX sticky semantics")
        import private_root as pr

        shared = Path(tempfile.gettempdir())
        mode = stat.S_IMODE(shared.lstat().st_mode)
        if not mode & 0o022 or not shared.lstat().st_mode & stat.S_ISVTX:
            self.skipTest("this host's temporary directory is not sticky-shared")
        pr.assert_path_cannot_reenter(shared)

    def test_a_base_with_modify_for_everyone_is_refused(self) -> None:
        # Round 8: `Modify` carries `DELETE` on the object, so another user can
        # rename the *base itself* and put a link in its place. It is therefore no
        # longer an acceptable base, even though it cannot delete our child.
        parent = self.base / "modify-parent"
        parent.mkdir()
        self._make_root_writable_by_everyone(parent, rights="M")
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(parent, "modify-base")
        self.assertFalse((parent / "goallatch-h5d-modify-base").exists())

    def test_an_inherit_only_owner_ace_leaves_the_root_unusable(self) -> None:
        # Round 8 (Medium): an owner ACE that is inherit-only grants nothing on the
        # root itself, so accepting it would declare an unusable root private. The
        # DACL is rebuilt with only that ACE, because `icacls /grant:r` adds a
        # second ACE instead of replacing ours.
        if os.name != "nt":
            self.skipTest("Windows inheritance flags")
        import subprocess as sp

        import private_root as pr

        parent = self.base / "inherit-only-parent"
        parent.mkdir()
        root = parent / "root"
        root.mkdir()
        owner = self._owner_sid()
        for arguments in (
            ["/inheritance:r"],
            ["/remove:g", f"*{owner}"],
            ["/grant", f"*{owner}:(OI)(CI)(IO)(F)"],
        ):
            completed = sp.run(
                ["icacls", str(root), *arguments],
                capture_output=True,
                text=True,
                check=False,
            )
            if completed.returncode != 0:
                self.skipTest(f"icacls is unavailable: {completed.stderr.strip()}")
        with self.assertRaises(pr.PrivateRootError):
            pr.assert_private_root(root)

    def test_a_path_that_resolves_to_the_home_is_refused(self) -> None:
        # Round 9 (High): a lexical path such as `<home>/x/..` passes a textual
        # home check and then resolves to the home itself, so the resolved location
        # is judged too.
        sneaky = TEST_FIXTURE_ROOT / ".."
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(sneaky, "resolve-to-home")

    def test_a_live_run_keeps_its_root_to_itself(self) -> None:
        # Round 10 (Medium): a second run on the same run id would rewrite the
        # seeded workspace under the first one, so the root is leased exclusively.
        fixture = driver.make_fixture(self.base, "live-lease")
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "live-lease")
        fixture.release()
        again = driver.make_fixture(self.base, "live-lease")
        self.assertEqual(fixture.root, again.root)

    def test_an_attempt_directory_is_never_shared(self) -> None:
        # Round 9 (Medium): two runs reusing one root must not land in the same
        # attempt directory and interleave their evidence.
        import private_root as pr

        fixture = driver.make_fixture(self.base, "attempt-exclusive")
        with self.assertRaises(FileExistsError):
            pr.create_private_dir_exclusive(fixture.attempt_dir)

    def test_no_fixture_base_is_offered_by_default(self) -> None:
        # The OS temporary directory is not an implicit default any more: on a
        # shared machine it cannot be proven private, so the operator must choose.
        with self.assertRaises(driver.FixtureSafetyError) as raised:
            driver.make_fixture(None, "no-base")
        self.assertIn("private directory", str(raised.exception))

    def test_a_user_shell_folder_is_refused_as_a_base(self) -> None:
        # The home tree is now allowed for a dedicated directory, so the usual
        # "typed path" accident is still refused where it would hurt.
        for name in ("Documents", "Desktop", "Downloads"):
            candidate = Path.home() / name
            with self.assertRaises(driver.FixtureSafetyError):
                driver.make_fixture(candidate, "shell-folder")

    def test_reusing_a_root_that_became_writable_by_others_is_refused(self) -> None:
        owned = driver.make_fixture(self.base, "tampered-root")
        self._make_root_writable_by_everyone(owned.root)
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "tampered-root")

    def test_a_group_writable_root_is_refused_on_posix(self) -> None:
        if os.name == "nt":
            self.skipTest("POSIX mode check; Windows uses the DACL")
        owned = driver.make_fixture(self.base, "group-writable")
        os.chmod(owned.root, 0o750)
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "group-writable")

    def test_an_owner_only_root_is_accepted_on_every_platform(self) -> None:
        owned = driver.make_fixture(self.base, "owner-only-reuse")
        import private_root as pr

        pr.assert_private_root(owned.root)
        owned.release()
        again = driver.make_fixture(self.base, "owner-only-reuse")
        self.assertEqual(owned.root, again.root)

    def test_a_root_whose_write_aces_do_not_inherit_is_refused(self) -> None:
        # Review round 6 (High): the root's own DACL proves nothing about its
        # children unless the write ACEs carry object+container inheritance.
        if os.name != "nt":
            self.skipTest("Windows inheritance flags")
        import subprocess as sp

        import private_root as pr

        fixture = driver.make_fixture(self.base, "non-inheritable")
        owner = self._owner_sid()
        completed = sp.run(
            ["icacls", str(fixture.root), "/grant:r", f"*{owner}:(F)"],
            capture_output=True,
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            self.skipTest(f"icacls is unavailable: {completed.stderr.strip()}")
        with self.assertRaises(pr.PrivateRootError):
            pr.assert_private_root(fixture.root)
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "non-inheritable")

    def test_a_root_ace_that_stops_inheriting_is_refused(self) -> None:
        # Round 11: OI|CI plus NO_PROPAGATE_INHERIT stops after one generation, so a
        # grandchild would fall back to the environment's default DACL.
        if os.name != "nt":
            self.skipTest("Windows inheritance flags")
        import subprocess as sp

        import private_root as pr

        parent = self.base / "no-propagate-parent"
        parent.mkdir()
        root = parent / "root"
        root.mkdir()
        owner = self._owner_sid()
        for arguments in (
            ["/inheritance:r"],
            ["/grant:r", f"*{owner}:(OI)(CI)(NP)(F)"],
            ["/grant", "*S-1-5-18:(OI)(CI)(F)"],
            ["/grant", "*S-1-5-32-544:(OI)(CI)(F)"],
        ):
            completed = sp.run(
                ["icacls", str(root), *arguments],
                capture_output=True,
                text=True,
                check=False,
            )
            if completed.returncode != 0:
                self.skipTest(f"icacls is unavailable: {completed.stderr.strip()}")
        with self.assertRaises(pr.PrivateRootError):
            pr.assert_private_root(root)

    def test_a_bad_skeleton_is_refused_before_any_seeded_file_is_rewritten(self) -> None:
        # Round 11: verification must happen before seeding, so a refused reuse
        # cannot truncate or rewrite the previous run's workspace.
        fixture = driver.make_fixture(self.base, "skeleton-order")
        fixture.release()
        seeded = fixture.workspace / "src" / "main.rs"
        before = seeded.read_text(encoding="utf-8")
        if os.name == "nt":
            import subprocess as sp

            owner = self._owner_sid()
            completed = sp.run(
                ["icacls", str(fixture.workspace / "src"), "/grant:r", f"*{owner}:(F)"],
                capture_output=True,
                text=True,
                check=False,
            )
            if completed.returncode != 0:
                self.skipTest(f"icacls is unavailable: {completed.stderr.strip()}")
        else:
            os.chmod(fixture.workspace / "src", 0o755)
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "skeleton-order")
        self.assertEqual(seeded.read_text(encoding="utf-8"), before)

    def test_a_child_that_became_writable_by_others_is_refused(self) -> None:
        # Review round 6 (High): a child's DACL can diverge from the root's, so the
        # whole tree is verified, not a sample of it.
        import private_root as pr

        fixture = driver.make_fixture(self.base, "tampered-child")
        self._make_root_writable_by_everyone(fixture.state / "sessions")
        with self.assertRaises(pr.PrivateRootError):
            pr.verify_private_tree(fixture.root)
        with self.assertRaises(driver.FixtureSafetyError):
            driver.make_fixture(self.base, "tampered-child")

    def _make_root_writable_by_everyone(self, root: Path, rights: str = "M") -> None:
        """Grant a real second principal (Everyone) `rights` on `root`.

        `M` (Modify) gives write access but no `FILE_DELETE_CHILD`, so it cannot
        move a protected child aside. `F` (full control) can, and is what the
        parent-entry test needs.
        """
        if os.name == "nt":
            import subprocess as sp

            completed = sp.run(
                [
                    "icacls",
                    str(root),
                    "/grant",
                    f"*S-1-1-0:(OI)(CI)({rights})",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if completed.returncode != 0:
                self.skipTest(f"icacls is unavailable: {completed.stderr.strip()}")
            return
        os.chmod(root, 0o777)



class AnswerParsingTests(unittest.TestCase):
    def test_the_first_balanced_object_is_extracted(self) -> None:
        answer = driver.extract_answer('prefix {"a": {"b": 1}} suffix {"c": 2}')
        self.assertEqual(answer, {"a": {"b": 1}})

    def test_braces_inside_strings_do_not_confuse_the_parser(self) -> None:
        answer = driver.extract_answer('{"summary": "a } brace and { brace"}')
        self.assertEqual(answer, {"summary": "a } brace and { brace"})

    def test_an_escaped_quote_does_not_confuse_the_parser(self) -> None:
        answer = driver.extract_answer('{"summary": "a \\" quoted } brace"}')
        self.assertEqual(answer, {"summary": 'a " quoted } brace'})

    def test_missing_json_is_classified(self) -> None:
        with self.assertRaises(driver.ModelInvocationError) as raised:
            driver.extract_answer("I could not answer.")
        self.assertEqual(raised.exception.classification, "MODEL_OUTPUT_NO_JSON")

    def test_empty_output_is_classified(self) -> None:
        with self.assertRaises(driver.ModelInvocationError) as raised:
            driver.extract_answer("   \n")
        self.assertEqual(raised.exception.classification, "MODEL_OUTPUT_EMPTY")

    def test_invalid_json_is_classified(self) -> None:
        with self.assertRaises(driver.ModelInvocationError) as raised:
            driver.extract_answer('{"summary": }')
        self.assertEqual(raised.exception.classification, "MODEL_OUTPUT_INVALID_JSON")

    def test_a_json_array_is_not_an_answer_object(self) -> None:
        # `[1]` contains no balanced object, so it is a no-JSON answer.
        with self.assertRaises(driver.ModelInvocationError) as raised:
            driver.extract_answer("[1, 2]")
        self.assertEqual(raised.exception.classification, "MODEL_OUTPUT_NO_JSON")


class RedactionTests(unittest.TestCase):
    def test_credential_shapes_are_masked(self) -> None:
        raw = (
            "Authorization: Bearer abcdefghijklmnop\n"
            "token=ghp_0123456789abcdefghijABCDEFGHIJ\n"
            'api_key="sk-abcdefghijklmnopqrstuvwx"\n'
        )
        redacted = driver.redact(raw)
        self.assertNotIn("abcdefghijklmnop", redacted)
        self.assertNotIn("ghp_0123456789abcdefghijABCDEFGHIJ", redacted)
        self.assertNotIn("sk-abcdefghijklmnopqrstuvwx", redacted)
        self.assertIn("<redacted", redacted)

    def test_ordinary_text_is_unchanged(self) -> None:
        text = "resolve_needs_replan is a JSON array of existing Task id strings"
        self.assertEqual(driver.redact(text), text)


class PromptForwardingTests(DisposableBase):
    def test_notes_reach_the_prompt(self) -> None:
        log_dir = self.base / "logs"
        prompt = driver.PROMPT.format(
            contract="{}",
            request="{}",
            conditions=driver.condition_lines(
                [{"code": "C1", "detail": "condition detail"}]
            ),
            notes=driver.note_lines(["a host note"]),
        )
        self.assertIn("C1: condition detail", prompt)
        self.assertIn("a host note", prompt)
        log_dir.mkdir()

    def test_an_empty_note_list_is_stated_explicitly(self) -> None:
        self.assertIn("no additional notes", driver.note_lines([]))
        self.assertIn("no additional notes", driver.note_lines(None))


if __name__ == "__main__":
    unittest.main(verbosity=2)
