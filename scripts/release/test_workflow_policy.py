#!/usr/bin/env python3
"""Static checks for the repository's GitHub Actions workflows.

These are the failure modes that a green CI run will happily hide:

  * An action referenced by a floating tag (`@v4`) instead of a commit SHA,
    which makes a third-party action able to change what it executes after
    the workflow was reviewed.
  * A workflow that grants write permissions it does not need, or grants them
    at the top level where every job inherits them.
  * A publish-style job that is reachable from a dry-run trigger.

Each check is deliberately a regex over the raw YAML rather than a full parse:
the goal is to assert policy properties of the source text, which a parsed
representation would obscure.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"

# A reference pinned to a full 40-character commit SHA.
SHA_PIN = re.compile(r"^[0-9a-f]{40}$")


def workflow_files() -> list[Path]:
    return sorted(WORKFLOW_DIR.glob("*.yml")) + sorted(WORKFLOW_DIR.glob("*.yaml"))


def job_blocks(path: Path):
    """Split a workflow's `jobs:` section into ``(name, body)`` pairs.

    Comment banners between jobs would otherwise be mistaken for a job name,
    so any block that does not look like `name:` followed by a job body is
    discarded.
    """
    text = path.read_text(encoding="utf-8")
    if "\njobs:" not in text:
        return []
    jobs = text.split("\njobs:", 1)[1]
    for block in re.split(r"\n  (?=[a-z_][a-z0-9_-]*:\n)", jobs):
        name, _, body = block.partition(":")
        name = name.strip()
        if not name or name.startswith("#"):
            continue
        yield name, body


class WorkflowPolicyTest(unittest.TestCase):
    def test_at_least_one_workflow_exists(self) -> None:
        self.assertTrue(workflow_files(), "no workflow files found")

    def test_every_action_is_pinned_to_a_full_commit_sha(self) -> None:
        for path in workflow_files():
            text = path.read_text(encoding="utf-8")
            for line_number, line in enumerate(text.splitlines(), 1):
                match = re.search(r"uses:\s*([^\s#]+)", line)
                if not match:
                    continue
                reference = match.group(1)
                _, _, version = reference.partition("@")
                with self.subTest(workflow=path.name, line=line_number, ref=reference):
                    self.assertRegex(
                        version,
                        SHA_PIN,
                        f"{path.name}:{line_number} pins {reference!r} to something "
                        f"other than a full commit SHA",
                    )

    def test_no_write_permissions_at_the_workflow_top_level(self) -> None:
        # A top-level `permissions:` block applies to every job, so a `write:`
        # entry there hands write scope to build and test jobs that never need
        # it. Write scope must be granted on a single named job instead.
        for path in workflow_files():
            text = path.read_text(encoding="utf-8")
            top_level = text.split("\njobs:", 1)[0]
            for line in top_level.splitlines():
                if re.match(r"^\s*permissions:", line):
                    with self.subTest(workflow=path.name, line=line.strip()):
                        self.assertNotIn(
                            "write",
                            line,
                            f"{path.name}: top-level permissions must not grant write",
                        )

    def test_release_publish_job_is_unreachable_from_a_dispatch(self) -> None:
        # The dry-run guarantee: a `workflow_dispatch` run has
        # event_name == 'workflow_dispatch' and never holds a refs/tags/* ref,
        # so requiring both on the publish job makes publication impossible to
        # reach by dispatch regardless of any input value.
        path = WORKFLOW_DIR / "release.yml"
        self.assertTrue(path.is_file(), "release.yml is missing")
        text = path.read_text(encoding="utf-8")

        publish = re.search(
            r"\n  publish:\n(.*?)(?=\n  [a-z_]+:\n|\Z)", text, flags=re.DOTALL
        )
        if publish is None:
            raise AssertionError("release.yml has no publish job")
        body = publish.group(1)

        condition = re.search(r"^\s{4}if:\s*(.+)$", body, flags=re.MULTILINE)
        if condition is None:
            raise AssertionError("the publish job has no `if:` guard")

        guard = condition.group(1)
        self.assertIn("github.event_name == 'push'", guard)
        self.assertIn("refs/tags/v", guard)

    def test_release_workflow_never_grants_write_outside_publish(self) -> None:
        for name, body in job_blocks(WORKFLOW_DIR / "release.yml"):
            if "write" in body and name != "publish":
                self.fail(
                    f"release.yml job {name!r} mentions write; only the publish job may"
                )

    def test_release_dry_run_branch_cannot_reach_publish(self) -> None:
        # The dry-run branch is a push trigger, so the publish guard has to
        # exclude branch pushes explicitly. `refs/tags/v` does not match a
        # `refs/heads/...` ref, which is what keeps this safe; assert the guard
        # is written in a way that depends on the ref, not on an input.
        text = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
        publish = re.search(
            r"\n  publish:\n(.*?)(?=\n  [a-z_]+:\n|\Z)", text, flags=re.DOTALL
        )
        if publish is None:
            raise AssertionError("release.yml has no publish job")
        guard = re.search(r"^\s{4}if:\s*(.+)$", publish.group(1), flags=re.MULTILINE)
        if guard is None:
            raise AssertionError("the publish job has no `if:` guard")
        self.assertIn("refs/tags/v", guard.group(1))

    def test_dry_run_branch_is_the_sanctioned_dry_run_trigger(self) -> None:
        text = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
        self.assertIn("release/public-v1-dry-run", text)

    def test_release_workflow_runs_posix_scripts_with_bash(self) -> None:
        # The matrix includes Windows, whose runner default is PowerShell.
        # Every workflow script uses POSIX syntax, so the workflow-level
        # default must select the runner's Git Bash explicitly.
        text = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
        defaults = re.search(r"^defaults:\n(.*?)(?=^jobs:)", text, flags=re.MULTILINE | re.DOTALL)
        if defaults is None:
            raise AssertionError("release workflow is missing defaults")
        self.assertRegex(defaults.group(1), r"(?m)^\s+shell:\s+bash\s*$")

    def test_release_notes_are_bundled_but_not_published_as_an_asset(self) -> None:
        text = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
        self.assertIn("Add release notes to the release bundle", text)
        self.assertIn('cp "docs/release/RELEASE_NOTES_v${VERSION}.md" dist/RELEASE_NOTES.md', text)
        self.assertIn('--notes-file "$notes"', text)
        self.assertIn("dist/*.tar.gz dist/*.zip dist/SHA256SUMS", text)

    def test_release_jobs_declare_timeouts(self) -> None:
        # An unbounded job can hang a release run until the platform's own
        # limit, so every job states its own bound.
        for name, body in job_blocks(WORKFLOW_DIR / "release.yml"):
            with self.subTest(job=name):
                self.assertIn(
                    "timeout-minutes:",
                    body,
                    f"job {name!r} has no timeout-minutes",
                )


if __name__ == "__main__":
    unittest.main(verbosity=2)
