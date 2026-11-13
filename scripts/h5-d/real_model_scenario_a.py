#!/usr/bin/env python3
"""H5-D — real-model end-to-end driver for the GoalLatch external backend.

This is the harness, not a fixture replay. For every `ModelWorkItem` the host
issues, it asks a real model (Codex `gpt-6.1-terra` through `codex exec`) for an
answer, and submits exactly what the model produced. Nothing the model "would
have said" is synthesised here.

What is real in a run of this script:

* a real `local-mcp mcp` child process, driven over the same newline-delimited
  JSON-RPC transport a normal MCP client uses;
* a real durable Goal in an **isolated** state root (`LOCAL_MCP_STATE_DIR`
  pointing at a throwaway directory) and a throwaway workspace;
* real host-issued `ModelWorkItem`s with real `request_digest`s and the H5-B
  machine-readable `response_contract`;
* real model inference (`codex exec`, sandbox `read-only`, its own process);
* real host validation of the model's answer, including an intentional
  out-of-contract answer used as a negative control.

H5.3-0 driver safety (see docs/campaign/H5_3_0_DRIVER_SAFETY.md):
the fixture is **created, never deleted**. Every run gets a unique root under the
OS temporary directory; an existing root is refused unless it carries an
ownership marker that matches the requested run id, and per-run logs and evidence
live in `attempts/<n>/` so a previous run is never overwritten. Symlinks,
Windows junctions and other reparse points are refused, and the fixture root may
not live inside a Git work tree, the user's home directory or a drive root.

Usage:
    python scripts/h5-d/real_model_scenario_a.py --work-dir tmp/h5-d/scenario-a
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import private_root  # noqa: E402  (H5.3.1 owner-only root creation)


# ---------------------------------------------------------------------------
# MCP transport
# ---------------------------------------------------------------------------


class McpServer:
    """A real `local-mcp mcp` process over stdio."""

    def __init__(self, binary: Path, state: Path, cwd: Path):
        self.log: list[dict] = []
        env = dict(os.environ)
        env["LOCAL_MCP_STATE_DIR"] = str(state)
        self.child = subprocess.Popen(
            [str(binary), "mcp"],
            cwd=str(cwd),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self._id = 0
        initialized = self.request("initialize", {"protocolVersion": "2025-06-18"})
        name = initialized["result"]["serverInfo"]["name"]
        assert name == "local-mcp", f"unexpected server identity {name!r}"

    def request(self, method: str, params: dict) -> dict:
        self._id += 1
        request_id = self._id
        line = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        )
        assert self.child.stdin is not None
        self.child.stdin.write(line + "\n")
        self.child.stdin.flush()
        assert self.child.stdout is not None
        raw = self.child.stdout.readline()
        assert raw, f"the server closed the transport during {method}"
        parsed = json.loads(raw)
        assert parsed.get("id") == request_id, f"id mismatch: {parsed}"
        self.log.append({"method": method, "params": params, "response": parsed})
        return parsed

    def call(self, name: str, arguments: dict) -> dict:
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        assert "error" not in response, f"transport error from {name}: {response}"
        result = response["result"]
        if result.get("isError") is True:
            text = result["content"][0]["text"]
            raise RuntimeError(f"{name} refused: {text}")
        text = result["content"][0]["text"]
        return json.loads(text)

    def close(self) -> None:
        if self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.kill()


# ---------------------------------------------------------------------------
# Fixture (H5.3-0: created, never deleted)
# ---------------------------------------------------------------------------

FIXTURE_MARKER_NAME = "goallatch-fixture-owner.json"
FIXTURE_MARKER_SCHEMA = "goallatch.h5d.fixture-owner.v1"
_FILE_ATTRIBUTE_REPARSE_POINT = 0x400


class FixtureSafetyError(RuntimeError):
    """The requested fixture root cannot be created or reused safely."""


@dataclass(frozen=True)
class Fixture:
    """A disposable, uniquely-owned run root."""

    run_id: str
    root: Path
    state: Path
    workspace: Path
    attempt_dir: Path
    session_id: str

    @property
    def logs(self) -> Path:
        return self.attempt_dir / "logs"

    @property
    def evidence_path(self) -> Path:
        return self.attempt_dir / "evidence.json"


def _is_reparse_point(path: Path) -> bool:
    """A symlink, Windows junction, or any other reparse point."""
    try:
        info = path.lstat()
    except OSError:
        return False
    if stat.S_ISLNK(info.st_mode):
        return True
    return bool(getattr(info, "st_file_attributes", 0) & _FILE_ATTRIBUTE_REPARSE_POINT)


def _reparse_components(path: Path, stop: Path) -> list[Path]:
    """Every existing reparse point on the `stop`..`path` segment, inclusive."""
    flagged: list[Path] = []
    current = path
    while True:
        if current.exists() and _is_reparse_point(current):
            flagged.append(current)
        if current == stop or current.parent == current:
            return flagged
        current = current.parent


def _assert_disposable_base(base: Path) -> Path:
    """Canonicalise the fixture base and refuse anything that is not disposable.

    Refused: the user's home directory, a drive root, anything inside a Git work
    tree (the primary checkout and every registered worktree carry a `.git`
    file or directory at their root), and any reparse-pointed segment.
    """
    if not base.is_absolute():
        raise FixtureSafetyError(
            f"--work-dir must be absolute, got {base}; relative roots could delete "
            "an operator directory"
        )
    base = base.parent if base.name in ("", base.anchor) else base
    base.mkdir(parents=True, exist_ok=True)
    canonical = base.resolve()

    home = Path.home().resolve()
    if canonical == home or canonical == Path(canonical.anchor):
        raise FixtureSafetyError(f"{canonical} is a home directory or drive root")
    if home in canonical.parents:
        # Inside the home tree is allowed only below the OS temporary directory;
        # a home-rooted fixture is never a deliberate campaign target.
        temporary = Path(tempfile.gettempdir()).resolve()
        if canonical != temporary and temporary not in canonical.parents:
            raise FixtureSafetyError(
                f"{canonical} is inside the user home directory and outside the "
                "OS temporary directory"
            )
    if _reparse_components(canonical, Path(canonical.anchor)):
        raise FixtureSafetyError(f"{canonical} is reached through a reparse point")
    for ancestor in (canonical, *canonical.parents):
        if (ancestor / ".git").exists():
            raise FixtureSafetyError(
                f"{canonical} is inside the Git work tree at {ancestor}"
            )
    return canonical


def _assert_private_fixture_root(root: Path) -> None:
    """A fixture root must not be writable by anyone but its owner.

    Review round 5 (R1b): a group- or world-writable root lets another user
    replace a fixture file with a link between the hazard scan and the write, and
    the harness would then write with the operator's authority. H5.3 answered
    this on POSIX only and returned early on Windows, because `%TEMP%` was
    assumed to be per-user. H5.3.1 measured the campaign host and found the
    assumption false (another local SID and a sandbox group hold `Modify` on
    `%TEMP%`), so Windows is now checked too: the whole DACL is read and any
    writer outside the owner, SYSTEM and Administrators refuses the root.

    A root that fails the check is refused rather than repaired, because
    changing another directory's permissions is not this harness's decision.
    """
    try:
        private_root.assert_private_root(root)
    except private_root.PrivateRootError as error:
        raise FixtureSafetyError(str(error)) from error


def _write_marker(root: Path, run_id: str) -> None:
    marker = {
        "schema": FIXTURE_MARKER_SCHEMA,
        "run_id": run_id,
        "canonical_root": str(root.resolve()),
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "owner_pid": os.getpid(),
        "note": "H5.3-0: a root without this marker is never reused or removed",
    }
    (root / FIXTURE_MARKER_NAME).write_text(
        json.dumps(marker, indent=2), encoding="utf-8"
    )


def _claim_existing_root(root: Path, run_id: str) -> None:
    """Prove that an existing root is the one this run id already owns."""
    marker_path = root / FIXTURE_MARKER_NAME
    if _is_reparse_point(root) or _is_reparse_point(marker_path):
        raise FixtureSafetyError(f"{root} or its marker is a reparse point")
    if not marker_path.is_file():
        raise FixtureSafetyError(
            f"{root} exists and carries no {FIXTURE_MARKER_NAME}; refusing to touch it"
        )
    try:
        marker = json.loads(marker_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise FixtureSafetyError(f"{marker_path} is unreadable: {error}") from error
    if marker.get("schema") != FIXTURE_MARKER_SCHEMA:
        raise FixtureSafetyError(f"{marker_path} has an unknown schema")
    if marker.get("run_id") != run_id:
        raise FixtureSafetyError(
            f"{marker_path} belongs to run {marker.get('run_id')!r}, not {run_id!r}"
        )
    if marker.get("canonical_root") != str(root.resolve()):
        raise FixtureSafetyError(
            f"{root} resolves to {root.resolve()}, not the owned "
            f"{marker.get('canonical_root')!r}"
        )


def _reused_root_hazards(root: Path) -> list[tuple[Path, str]]:
    """Hazards already inside `root` that reuse would write or read through.

    Reuse must not be a way to smuggle an indirection in. Three kinds matter:

    * a symlink or Windows junction/reparse point (write and read escape);
    * a hard link (not a reparse point at all: `write_text` truncates the shared
      file, so the fixture would overwrite a file outside the root);
    * an unreadable directory (the scan cannot prove it is safe, so it is
      refused).

    The walk has no depth limit and never descends into a link, so the answer
    does not depend on how deep the hazard is.
    """
    hazards: list[tuple[Path, str]] = []
    stack: list[Path] = [root]
    while stack:
        path = stack.pop()
        try:
            entries = list(path.iterdir())
        except OSError:
            hazards.append((path, "UNREADABLE_DIRECTORY"))
            continue
        for entry in entries:
            if _is_reparse_point(entry):
                hazards.append((entry, "REPARSE_POINT"))
                continue
            try:
                info = entry.lstat()
            except OSError:
                hazards.append((entry, "UNREADABLE"))
                continue
            if stat.S_ISDIR(info.st_mode):
                stack.append(entry)
            elif stat.S_ISREG(info.st_mode) and info.st_nlink > 1:
                hazards.append((entry, "HARD_LINK"))
    return hazards


def _existing_reparse_points(root: Path) -> list[Path]:
    """Backwards-compatible view of `_reused_root_hazards` (paths only)."""
    return [path for path, _kind in _reused_root_hazards(root)]


def _refuse_reused_root_hazards(root: Path) -> None:
    """Fail closed when a reused root already contains an indirection.

    The check and the fixture writes are still separate filesystem operations, so
    a process that can create entries inside this root *between* the two could
    still race them. That is not defended against here and cannot be: a writer
    inside a fixture root on the same OS user is the same-uid boundary recorded as
    C3/R-1. What this does guarantee is that an operator cannot point the harness
    at a root that already redirects somewhere else.
    """
    hazards = _reused_root_hazards(root)
    if hazards:
        raise FixtureSafetyError(
            f"{root} already contains filesystem indirections, so fixture writes "
            f"could escape it: "
            f"{[(str(path), kind) for path, kind in hazards]}"
        )


def _next_attempt_dir(root: Path) -> Path:
    """A per-run, never-reused directory for logs and evidence."""
    attempts = root / "attempts"
    attempts.mkdir(exist_ok=True)
    used = [
        int(child.name)
        for child in attempts.iterdir()
        if child.is_dir() and child.name.isdigit()
    ]
    attempt = attempts / str((max(used) + 1) if used else 1)
    attempt.mkdir()
    return attempt


def make_fixture(
    work_dir: Path | None = None, run_id: str | None = None
) -> Fixture:
    """Create a throwaway state root, workspace and Session record.

    Nothing is ever deleted. A root that already exists is either claimed (when
    its ownership marker matches `run_id`) or refused, and every run writes into
    its own `attempts/<n>` directory.
    """
    base = _assert_disposable_base(
        Path(work_dir) if work_dir is not None else Path(tempfile.gettempdir())
    )
    if run_id is None:
        run_id = (
            time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
            + "-"
            + uuid.uuid4().hex[:8]
        )
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,96}", run_id):
        raise FixtureSafetyError(f"unsafe run id {run_id!r}")

    root = base / f"goallatch-h5d-{run_id}"
    if root.exists():
        # Prove the root is a plain, indirection-free tree *before* reading the
        # ownership marker: the marker itself must not be reachable through a
        # hard link or a reparse point, or a foreign file could impersonate it.
        if _reparse_components(root, base):
            raise FixtureSafetyError(f"{root} is reached through a reparse point")
        _refuse_reused_root_hazards(root)
        _assert_private_fixture_root(root)
        _claim_existing_root(root, run_id)
    else:
        # Created *with* an owner-only descriptor, not created and then hardened:
        # there is no window in which the inherited, possibly shared parent ACL
        # applies. A failure to apply or verify it refuses the root (fail closed).
        try:
            private_root.create_owner_only_dir(root)
        except private_root.PrivateRootError as error:
            raise FixtureSafetyError(str(error)) from error
        _write_marker(root, run_id)

    if _reparse_components(root, base):
        raise FixtureSafetyError(f"{root} is reached through a reparse point")
    hazards = _reused_root_hazards(root)
    if hazards:
        raise FixtureSafetyError(
            f"{root} already contains filesystem indirections, so fixture writes "
            f"could escape it: "
            f"{[(str(path), kind) for path, kind in hazards]}"
        )
    if base.resolve() not in root.resolve().parents:
        raise FixtureSafetyError(f"{root} escapes the disposable base {base}")

    state = root / "state"
    repo = root / "repo"
    for path in (state / "sessions", repo / "src", repo / "data"):
        path.mkdir(parents=True, exist_ok=True)
    (repo / "src" / "main.rs").write_text(
        'fn main() {\n    println!("goal-latch h5-d fixture");\n}\n', encoding="utf-8"
    )
    (repo / "src" / "helper.rs").write_text(
        "pub fn helper() -> u32 {\n    1\n}\n", encoding="utf-8"
    )
    (repo / "data" / "inventory.txt").write_text(
        "widgets=42\ngadgets=7\nspares=0\n", encoding="utf-8"
    )
    # H5.3.1: the guarantee is about the *contents* of the root, not just its top
    # entry, so the inheritance that carries the owner-only DACL down is verified
    # on the entries this harness created. If the parent ACL somehow re-appeared
    # on a child, the run stops here rather than writing a model-controlled file
    # into it.
    for created in (
        root / FIXTURE_MARKER_NAME,
        state,
        state / "sessions",
        repo,
        repo / "src" / "main.rs",
        repo / "data" / "inventory.txt",
    ):
        try:
            private_root.assert_private_entry(created)
        except private_root.PrivateRootError as error:
            raise FixtureSafetyError(str(error)) from error

    session_id = f"h5d-{uuid.uuid4()}"
    record = {
        "id": session_id,
        "cwd": str(repo),
        "permitted_directories": [str(repo)],
    }
    (state / "sessions" / f"{session_id}.json").write_text(
        json.dumps(record, indent=2), encoding="utf-8"
    )
    return Fixture(
        run_id=run_id,
        root=root,
        state=state,
        workspace=repo,
        attempt_dir=_next_attempt_dir(root),
        session_id=session_id,
    )


def workspace_digest(repo: Path) -> dict[str, str]:
    """Hash every regular file under `repo` without following any indirection.

    `rglob` follows directory symlinks and `is_file()`/`read_bytes()` follow file
    symlinks, so a link inside a fixture could make the digest read bytes from
    outside it. Links are recorded as such, and a multiply linked file is marked
    instead of being read, because a hard link is an alias of a file elsewhere.
    """
    digests = {}
    for directory, subdirectories, filenames in os.walk(repo, followlinks=False):
        subdirectories[:] = [
            name
            for name in subdirectories
            if not _is_reparse_point(Path(directory) / name)
        ]
        for name in filenames:
            path = Path(directory) / name
            key = str(path.relative_to(repo)).replace("\\", "/")
            if _is_reparse_point(path):
                digests[key] = "REPARSE_POINT"
                continue
            try:
                info = path.lstat()
            except OSError:
                digests[key] = "UNREADABLE"
                continue
            if stat.S_ISREG(info.st_mode) and info.st_nlink > 1:
                digests[key] = "MULTIPLY_LINKED"
                continue
            try:
                digests[key] = hashlib.sha256(path.read_bytes()).hexdigest()
            except OSError:
                digests[key] = "UNREADABLE"
    return digests


# ---------------------------------------------------------------------------
# Real model
# ---------------------------------------------------------------------------

REDACTIONS: tuple[tuple[re.Pattern[str], str], ...] = (
    (re.compile(r"(?i)(authorization\s*[:=]\s*bearer\s+)\S+"), r"\1<redacted>"),
    (re.compile(r"(?i)\bgh[pousr]_[A-Za-z0-9]{20,}"), "<redacted-github-token>"),
    (re.compile(r"(?i)\bsk-[A-Za-z0-9_-]{16,}"), "<redacted-api-key>"),
    (
        re.compile(
            r'(?i)("?(?:api[_-]?key|access[_-]?token|secret|password)"?\s*[:=]\s*")[^"]+'
        ),
        r"\1<redacted>",
    ),
)


def redact(text: str) -> str:
    """Mask credential-shaped substrings before anything is written to disk."""
    for pattern, replacement in REDACTIONS:
        text = pattern.sub(replacement, text)
    return text


class ModelInvocationError(RuntimeError):
    """A classified real-model call failure. Never silently swallowed."""

    def __init__(self, classification: str, detail: str, meta: dict | None = None):
        super().__init__(f"{classification}: {detail}")
        self.classification = classification
        self.detail = detail
        self.meta = meta or {}


PROMPT = """You are answering a GoalLatch external ModelWorkItem.

Answer with exactly one JSON object and nothing else: no prose, no markdown
fence, no explanation before or after.

The host will refuse the submission if it violates this contract, so follow the
`schema` exactly, including the `status` vocabulary and the identity fields you
must echo:

{contract}

The host-authored request for this WorkItem is:

{request}

Rules that the schema cannot express (host-issued conditions, all mandatory):
{conditions}

Additional host notes for this role:
{notes}

Be concise. Do not invent facts you cannot observe in the workspace. If the
request asks you to inspect files, inspect them.
"""


def _first_json_object(text: str) -> str | None:
    """The first balanced `{...}` object, string- and escape-aware.

    A greedy regex would happily span two objects or swallow trailing prose; the
    host contract requires exactly one object.
    """
    start = text.find("{")
    while start != -1:
        depth = 0
        in_string = False
        escaped = False
        for index in range(start, len(text)):
            char = text[index]
            if in_string:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
                continue
            if char == '"':
                in_string = True
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    return text[start : index + 1]
        start = text.find("{", start + 1)
    return None


def extract_answer(stdout: str) -> dict:
    """Parse the model's single JSON answer object, or raise a classified error."""
    if not stdout.strip():
        raise ModelInvocationError("MODEL_OUTPUT_EMPTY", "the model wrote no stdout")
    candidate = _first_json_object(stdout)
    if candidate is None:
        raise ModelInvocationError(
            "MODEL_OUTPUT_NO_JSON",
            "the model produced no JSON object",
            {"tail": redact(stdout[-2000:])},
        )
    try:
        answer = json.loads(candidate)
    except ValueError as error:
        raise ModelInvocationError(
            "MODEL_OUTPUT_INVALID_JSON",
            f"the model's JSON object does not parse: {error}",
            {"candidate": redact(candidate[:2000])},
        ) from error
    if not isinstance(answer, dict):
        raise ModelInvocationError(
            "MODEL_OUTPUT_NOT_OBJECT",
            f"the model answered a {type(answer).__name__}, not an object",
        )
    return answer


def condition_lines(conditions: list[dict]) -> str:
    if not conditions:
        return "- (the host published no conditions for this role)"
    return "\n".join(
        f"- {entry['code']}: {entry['detail']}" for entry in conditions
    )


def note_lines(notes: list[str] | None) -> str:
    if not notes:
        return "- (the host published no additional notes for this role)"
    return "\n".join(f"- {note}" for note in notes)


def ask_model(
    repo: Path,
    contract: dict,
    request: dict,
    conditions: list[dict],
    log_dir: Path,
    step: int,
    model: str,
    notes: list[str] | None = None,
    timeout_seconds: int = 600,
    sandbox: str = "read-only",
) -> tuple[dict, dict]:
    """Ask the real model once. Return its answer and a classified call record."""
    prompt = PROMPT.format(
        contract=json.dumps(contract, indent=2),
        request=json.dumps(request, indent=2),
        conditions=condition_lines(conditions),
        notes=note_lines(notes),
    )
    log_dir.mkdir(parents=True, exist_ok=True)
    (log_dir / f"step-{step}-prompt.txt").write_text(
        redact(prompt), encoding="utf-8"
    )
    started = time.time()
    try:
        # Pass the prompt on stdin. A Replanner request carries the bounded plan
        # history, and a Windows command line is limited to ~32 KiB; stdin has
        # no such limit.
        completed = subprocess.run(
            [
                "codex",
                "exec",
                "--sandbox",
                sandbox,
                "--skip-git-repo-check",
                "-m",
                model,
                "-C",
                str(repo),
                "-",
            ],
            input=prompt,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout_seconds,
        )
    except FileNotFoundError as error:
        raise ModelInvocationError(
            "MODEL_CLI_MISSING", f"the codex CLI is not on PATH: {error}"
        ) from error
    except subprocess.TimeoutExpired as error:
        elapsed = time.time() - started
        (log_dir / f"step-{step}-model-stdout.txt").write_text(
            redact((error.stdout or "") if isinstance(error.stdout, str) else ""),
            encoding="utf-8",
        )
        raise ModelInvocationError(
            "MODEL_TIMEOUT",
            f"codex did not finish within {timeout_seconds}s",
            {"step": step, "elapsed_seconds": round(elapsed, 1)},
        ) from error

    elapsed = time.time() - started
    stdout = completed.stdout or ""
    (log_dir / f"step-{step}-model-stdout.txt").write_text(
        redact(stdout), encoding="utf-8"
    )
    (log_dir / f"step-{step}-model-stderr.txt").write_text(
        redact(completed.stderr or ""), encoding="utf-8"
    )
    meta = {
        "step": step,
        "model": f"codex {model} (codex exec, sandbox={sandbox})",
        "sandbox": sandbox,
        "model_id": model,
        "reasoning": "high" if "gpt-6.1-terra" in model else "cli-default",
        "exit_code": completed.returncode,
        "elapsed_seconds": round(elapsed, 1),
        "prompt_bytes": len(prompt),
        "conditions_forwarded": len(conditions or []),
        "notes_forwarded": len(notes or []),
    }
    if completed.returncode != 0:
        raise ModelInvocationError(
            "MODEL_EXIT_NONZERO",
            f"codex exited {completed.returncode}",
            {**meta, "stderr_tail": redact((completed.stderr or "")[-2000:])},
        )
    answer = extract_answer(stdout)
    meta["answer"] = answer
    meta["answer_class"] = "JSON_OBJECT"
    return answer, meta


# ---------------------------------------------------------------------------
# Drive
# ---------------------------------------------------------------------------


def drive(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    assert binary.is_file(), f"binary not found: {binary}"
    binary_sha = hashlib.sha256(binary.read_bytes()).hexdigest()

    fixture = make_fixture(
        Path(args.work_dir) if args.work_dir else None, args.run_id
    )
    root, state, repo, session_id = (
        fixture.root,
        fixture.state,
        fixture.workspace,
        fixture.session_id,
    )
    log_dir = fixture.logs
    log_dir.mkdir(parents=True, exist_ok=True)
    evidence: dict = {
        "scenario": "H5-D Scenario A (read-only DAG, real model)",
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "run_id": fixture.run_id,
        "fixture_root": str(root),
        "attempt_dir": str(fixture.attempt_dir),
        "binary": str(binary),
        "binary_sha256": binary_sha,
        "state_root": str(state),
        "workspace": str(repo),
        "session_id": session_id,
        "negative_control": None,
        "steps": [],
        "workspace_before": workspace_digest(repo),
    }
    workspace_before = dict(evidence["workspace_before"])

    server = McpServer(binary, state, repo)
    try:
        view = server.call(
            "goal_start",
            {
                "session_id": session_id,
                "objective": (
                    "Read-only survey of src/ and data/: report how many files under src/ "
                    "declare a function and the total of the numeric values in "
                    "data/inventory.txt. Do not modify anything."
                ),
                "model_backend": "EXTERNAL",
            },
        )
        goal_id = view["goal_id"]
        evidence["goal_id"] = goal_id
        evidence["goal_start"] = view

        model_calls = 0
        negative_control_done = False
        for step in range(args.max_steps):
            next_view = server.call(
                "goal_next", {"session_id": session_id, "goal_id": goal_id}
            )
            evidence["steps"].append(
                {
                    "step": step,
                    "outcome": next_view.get("outcome"),
                    "status": next_view.get("status"),
                    "revision": next_view.get("revision"),
                    "result_state": next_view.get("result_state"),
                    "next_action": next_view.get("next_action"),
                    "no_action_reason": next_view.get("no_action_reason"),
                    "blockers": next_view.get("blockers"),
                    "model_work_id": (next_view.get("model_work") or {}).get("work_id"),
                }
            )
            work = next_view.get("model_work")
            if work:
                contract = work.get("response_contract")
                assert contract, "H5-B: the WorkItem must publish a response_contract"
                evidence["steps"][-1]["contract_id"] = contract["contract_id"]
                evidence["steps"][-1]["schema_digest"] = contract["schema_digest"]

                # Negative control: the exact answer shape H4-D-F1 was about. The
                # host must refuse it and the published schema must already say so.
                if not negative_control_done:
                    negative_control_done = True
                    bogus = {
                        "goal_id": goal_id,
                        "goal_revision": work["goal_revision"],
                        "status": "COMPLETED",
                        "summary": "the model declares the Goal complete",
                        "tasks": [],
                    }
                    try:
                        result = server.call(
                            "goal_submit",
                            {
                                "session_id": session_id,
                                "goal_id": goal_id,
                                "work_id": work["work_id"],
                                "submission_id": str(uuid.uuid4()),
                                "expected_goal_revision": work["goal_revision"],
                                "request_digest": work["request_digest"],
                                "response": bogus,
                            },
                        )
                        evidence["negative_control"] = {
                            "submitted": bogus,
                            "outcome": result.get("outcome"),
                            "code": result.get("code"),
                        }
                    except RuntimeError as error:
                        evidence["negative_control"] = {
                            "submitted": bogus,
                            "refused": str(error),
                        }
                    # Re-issue: a refused submission does not consume the role.
                    next_view = server.call(
                        "goal_next", {"session_id": session_id, "goal_id": goal_id}
                    )
                    work = next_view.get("model_work")
                    contract = work["response_contract"]
                    evidence["steps"].append(
                        {
                            "step": f"{step}-reissued",
                            "outcome": next_view.get("outcome"),
                            "model_work_id": work["work_id"],
                            "contract_id": contract["contract_id"],
                        }
                    )

                answer, meta = ask_model(
                    repo,
                    contract["schema"],
                    work["request"],
                    contract["conditions"],
                    log_dir,
                    model_calls,
                    args.model,
                    notes=contract.get("notes"),
                    sandbox=getattr(args, "readonly_sandbox", "read-only"),
                )
                model_calls += 1
                evidence.setdefault("model_calls", []).append(meta)

                submission = {
                    "session_id": session_id,
                    "goal_id": goal_id,
                    "work_id": work["work_id"],
                    "submission_id": str(uuid.uuid4()),
                    "expected_goal_revision": work["goal_revision"],
                    "request_digest": work["request_digest"],
                    "response": answer,
                }
                try:
                    receipt = server.call("goal_submit", submission)
                except RuntimeError as error:
                    evidence.setdefault("submission_refusals", []).append(
                        {"role": work["role"], "error": str(error), "answer": answer}
                    )
                    raise
                evidence["steps"][-1]["role"] = work["role"]
                evidence["steps"][-1]["submission"] = {
                    "submission_id": submission["submission_id"],
                    "request_digest": submission["request_digest"],
                    "outcome": receipt.get("outcome"),
                    "code": receipt.get("code"),
                    "committed_revision": receipt.get("committed_revision"),
                }
                continue

            if next_view.get("status") in ("COMPLETED", "FAILED", "BLOCKED", "CANCELLED"):
                break
            if next_view.get("next_action") == "STOP_AND_RECONCILE":
                break
            time.sleep(0.2)

        final = server.call("goal_status", {"session_id": session_id, "goal_id": goal_id})
        evidence["final"] = final
        codex_spawns = 0
        evidence["codex_spawned_for_goal"] = codex_spawns
        evidence["workspace_after"] = workspace_digest(repo)
        evidence["workspace_unchanged"] = evidence["workspace_after"] == workspace_before
        evidence["model_call_count"] = model_calls
    finally:
        server.close()
        evidence["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        fixture.evidence_path.write_text(
            json.dumps(evidence, indent=2), encoding="utf-8"
        )

    print(json.dumps(
        {
            "evidence": str(fixture.evidence_path),
            "goal_id": evidence.get("goal_id"),
            "final_status": (evidence.get("final") or {}).get("status"),
            "model_calls": evidence.get("model_call_count"),
            "negative_control": evidence.get("negative_control", {}).get("outcome")
            if isinstance(evidence.get("negative_control"), dict)
            else None,
            "workspace_unchanged": evidence.get("workspace_unchanged"),
        },
        indent=2,
    ))
    ok = (evidence.get("final") or {}).get("status") == "COMPLETED" and evidence.get(
        "workspace_unchanged"
    )
    return 0 if ok else 1


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/local-mcp.exe")
    parser.add_argument(
        "--work-dir",
        default=None,
        help="absolute disposable base directory (default: the OS temp directory)",
    )
    parser.add_argument(
        "--run-id",
        default=None,
        help="reuse an owned run root; a colliding unowned root is refused",
    )
    parser.add_argument("--max-steps", type=int, default=24)
    # GoalLatch policy: never rely on the default Codex model. The campaign
    # explicitly dispatches gpt-6.1-terra; pass --model to override.
    parser.add_argument("--model", default="gpt-6.1-terra")
    parser.add_argument(
        "--readonly-sandbox",
        choices=("read-only", "workspace-write"),
        default="read-only",
        help=(
            "codex --sandbox for the model call. On Windows the 'read-only' mode "
            "makes the workspace unreadable to the model (see "
            "docs/campaign/H5_3_EVIDENCE.md); 'workspace-write' is an explicit, "
            "recorded deviation and every read-only call is followed by a "
            "workspace-digest check that proves the model wrote nothing."
        ),
    )
    return drive(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
