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
import subprocess
import unittest
from pathlib import Path
from urllib.parse import unquote, urlsplit

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


def trigger_block(path: Path) -> str:
    """Return the raw top-level `on:` block of a workflow.

    The block runs until the next column-0 key (or column-0 comment), which is
    enough separation to isolate the triggers from the rest of the header.
    """
    text = path.read_text(encoding="utf-8")
    match = re.search(r"^on:\n(.*?)(?=^\S)", text, flags=re.MULTILINE | re.DOTALL)
    if match is None:
        raise AssertionError(f"{path.name} has no top-level `on:` block")
    return match.group(0)


def dispatch_input_block(name: str) -> str:
    """Return the raw YAML body of one `workflow_dispatch` input."""
    triggers = trigger_block(WORKFLOW_DIR / "release.yml")
    match = re.search(
        rf"^      {name}:\n(.*?)(?=^      \w+:|^\S)", triggers, flags=re.MULTILINE | re.DOTALL
    )
    if match is None:
        raise AssertionError(f"release.yml has no workflow_dispatch input {name!r}")
    return match.group(1)


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

    def test_publish_guard_keys_on_the_tag_ref_not_on_an_input(self) -> None:
        # `refs/tags/v` cannot match a `refs/heads/...` ref or a
        # `workflow_dispatch` ref, so keying publication on the ref is what makes
        # it unreachable from every dry-run path. The guard must therefore be
        # written in terms of the ref, never in terms of a caller-supplied value.
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
        self.assertNotIn("inputs.", guard.group(1))

    def test_release_workflow_has_no_branch_push_dry_run_trigger(self) -> None:
        # The pre-release dry-run branch existed only while release.yml had not
        # yet reached the default branch. It is obsolete, and must stay gone: a
        # branch push is a third code path with no remaining purpose, and any
        # push to a same-named branch would silently re-enable it.
        #
        # This asserts on the `on:` block rather than the whole file, because the
        # trigger is the policy; the header comment is allowed to name the
        # retired branch when it explains why the trigger was removed.
        triggers = trigger_block(WORKFLOW_DIR / "release.yml")
        self.assertNotIn("release/public-v1-dry-run", triggers)
        self.assertNotRegex(triggers, r"(?m)^\s+branches:\s*$")

    def test_release_workflow_still_publishes_on_a_v_tag_push(self) -> None:
        # Removing the branch trigger must not cost the real publication path.
        triggers = trigger_block(WORKFLOW_DIR / "release.yml")
        push = re.search(
            r"^  push:\n(.*?)(?=^  \w+:|^\S)", triggers, flags=re.MULTILINE | re.DOTALL
        )
        if push is None:
            raise AssertionError("release.yml has no `push` trigger")
        self.assertIn('"v*"', push.group(1))
        self.assertNotIn("branches:", push.group(1))

    def test_release_workflow_still_supports_workflow_dispatch(self) -> None:
        triggers = trigger_block(WORKFLOW_DIR / "release.yml")
        self.assertIn("workflow_dispatch:", triggers)

    def test_dispatch_version_is_required_and_has_no_default(self) -> None:
        # A default would let an operator dry-run a stale version by clicking
        # through the form, which after v0.1.0 means re-running a version that is
        # already published. The version must be typed every time.
        body = dispatch_input_block("version")
        self.assertIn("required: true", body)
        # Match the `default:` key at the start of a line, not the substring:
        # the input's own description text mentions the word.
        self.assertIsNone(
            re.search(r"(?m)^\s+default:", body),
            "the version input must not carry a default",
        )

    def test_dispatch_input_does_not_hardcode_a_published_version(self) -> None:
        # Guards the general failure mode, not just the 0.1.0 instance: no
        # dispatch input may carry a version-shaped default.
        triggers = trigger_block(WORKFLOW_DIR / "release.yml")
        for default in re.findall(r"(?m)^\s+default:\s*[\"\']?([^\"\'\n]+)", triggers):
            self.assertIsNone(
                re.fullmatch(r"v?\d+\.\d+\.\d+", default.strip()),
                f"dispatch input defaults to the concrete version {default!r}",
            )

    def test_unsupported_trigger_fails_closed(self) -> None:
        # With the branch trigger removed there is no third case to resolve.
        # An unexpected trigger must stop the run rather than infer a version
        # from Cargo.toml, because a wrong version names every asset in the run.
        text = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
        plan = re.search(r"\n  plan:\n(.*?)(?=\n  [a-z_]+:\n|\Z)", text, flags=re.DOTALL)
        if plan is None:
            raise AssertionError("release.yml has no plan job")
        body = plan.group(1)
        fallback = re.search(r"^\s*else\n(.*?)^\s*fi\s*$", body, flags=re.MULTILINE | re.DOTALL)
        if fallback is None:
            raise AssertionError("the plan job has no else branch for unexpected triggers")
        else_block = fallback.group(1)
        self.assertIn("exit 1", else_block)
        self.assertIn("::error::", else_block)
        # The removed fallback resolved a version by inference; that must not
        # survive in any form, because it is what a stale branch push used.
        self.assertNotIn("dry_run=", else_block)
        self.assertNotIn("Cargo.toml", else_block)

    def test_release_workflow_top_level_permission_is_contents_read(self) -> None:
        # Not merely "no write at the top level": the whole workflow must still
        # declare exactly `contents: read`, so the publish job is the only thing
        # that can raise scope.
        text = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
        top_level = text.split("\njobs:", 1)[0]
        match = re.search(r"^permissions:\n(.*?)(?=^\S)", top_level, flags=re.MULTILINE | re.DOTALL)
        if match is None:
            raise AssertionError("release.yml declares no top-level permissions")
        declared = []
        for line in match.group(1).splitlines():
            if not line.strip():
                break
            if line.lstrip().startswith("#"):
                continue
            declared.append(line.strip())
        self.assertEqual(declared, ["contents: read"])

    def test_publish_is_the_only_job_that_grants_write(self) -> None:
        granting = [
            name for name, body in job_blocks(WORKFLOW_DIR / "release.yml")
            if re.search(r"^\s{4}permissions:\s*$", body, flags=re.MULTILINE)
            and re.search(r"^\s{6}\w+:\s*write\s*$", body, flags=re.MULTILINE)
        ]
        self.assertEqual(granting, ["publish"])

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
        self.assertIn('--title "Local MCP $TAG (Public v1)"', text)

    def test_release_notes_links_are_release_pinned_and_tracked(self) -> None:
        notes_path = Path("docs/release/RELEASE_NOTES_v0.1.0.md")
        notes = (REPO_ROOT / notes_path).read_text(encoding="utf-8")
        tracked = set(
            subprocess.check_output(
                ["git", "ls-files"], cwd=REPO_ROOT, text=True
            ).splitlines()
        )
        links = re.findall(r"!?\[[^\]]*\]\((<[^>]+>|[^)\s]+)", notes)
        local_targets = set()
        for raw_destination in links:
            destination = raw_destination.strip("<>")
            parsed = urlsplit(destination)
            if parsed.scheme in ("http", "https", "mailto"):
                if parsed.hostname == "github.com" and parsed.path.startswith(
                    "/Yrika819/local-mcp/"
                ):
                    prefix = "/Yrika819/local-mcp/blob/v0.1.0/"
                    self.assertTrue(parsed.path.startswith(prefix), destination)
                    target = unquote(parsed.path[len(prefix):])
                    self.assertIn(target, tracked, destination)
                    local_targets.add(target)
                continue

            self.fail(
                f"release notes must use tag-pinned absolute links, got {destination!r}"
            )

        self.assertEqual(
            local_targets,
            {
                "docs/linux_sandbox.md",
                "SECURITY.md",
                "docs/release/INSTALL.md",
                "LICENSE",
                "THIRD_PARTY_NOTICES.md",
            },
        )
        self.assertEqual(
            notes.count("Both architectures' executables dynamically link to"),
            1,
        )

    def test_release_document_relative_links_resolve_to_tracked_files(self) -> None:
        documents = (
            "README.md",
            "SECURITY.md",
            "THIRD_PARTY_NOTICES.md",
            "docs/linux_sandbox.md",
            "docs/release/INSTALL.md",
            "docs/release/RELEASE_NOTES_v0.1.0.md",
            "docs/release/RELEASE_CHECKLIST.md",
        )
        tracked = set(
            subprocess.check_output(
                ["git", "ls-files"], cwd=REPO_ROOT, text=True
            ).splitlines()
        )
        pattern = re.compile(r"!?\[[^\]]*\]\((<[^>]+>|[^)\s]+)")
        notes_path = "docs/release/RELEASE_NOTES_v0.1.0.md"
        for relative_document in documents:
            document = REPO_ROOT / relative_document
            text = document.read_text(encoding="utf-8")
            for raw_destination in pattern.findall(text):
                destination = raw_destination.strip("<>")
                parsed = urlsplit(destination)
                if parsed.scheme in ("http", "https", "mailto"):
                    continue
                if relative_document == notes_path:
                    self.fail(
                        f"release notes require absolute tag-pinned links: {destination!r}"
                    )
                target = (document.parent / unquote(parsed.path)).resolve()
                try:
                    tracked_path = target.relative_to(REPO_ROOT).as_posix()
                except ValueError:
                    self.fail(f"{relative_document}: link escapes repository: {destination!r}")
                self.assertIn(
                    tracked_path,
                    tracked,
                    f"{relative_document}: broken local link {destination!r}",
                )

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
