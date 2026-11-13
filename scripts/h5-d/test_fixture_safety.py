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
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import real_model_scenario_a as driver  # noqa: E402


class DisposableBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="h53-fixture-test-")
        self.base = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()


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
        again = driver.make_fixture(self.base, "owner-only-reuse")
        self.assertEqual(owned.root, again.root)

    def _make_root_writable_by_everyone(self, root: Path) -> None:
        if os.name == "nt":
            import subprocess as sp

            completed = sp.run(
                [
                    "icacls",
                    str(root),
                    "/grant",
                    "*S-1-1-0:(OI)(CI)(M)",
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
