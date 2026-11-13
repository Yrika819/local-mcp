#!/usr/bin/env python3
"""H5.2-B — real-model Writer/Reviewer end-to-end driver.

This drives the production External MCP surface over stdio against a throwaway
state root and workspace, asks a real Codex model for every `ModelWorkItem`, and
uses the H5-C `test-grant` fixture only to supply the Owner *decision* that the
Writer leg requires. Publication, the mutation journal, the Reviewer, the Task
Verifier, the Goal Verifier and the Finalizer are the production code paths.

Isolation:
* state root and workspace are siblings under one disposable parent;
* the state root carries a `test-grant-fixture.json` marker whose declared roots
  are the canonical state/workspace pair, and the parent is not a Git tree;
* the `test-grant` command exists only in the `test-grant-fixture` build and
  stamps `TEST_FIXTURE_V1`;
* the model runs with `codex exec --sandbox read-only -m gpt-6.1-terra` in the
  throwaway workspace.

Model budget: Planner + Writer + Reviewer = at most 3 inference calls.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import real_model_scenario_a as base  # noqa: E402


WRITE_SCOPE = {
    "allowed_paths": ["src/main.rs"],
    "allowed_operations": ["WRITE_UTF8"],
    "max_operations": 2,
    "max_bytes": 4096,
    "expires_in_seconds": 900,
}

NEW_CONTENT = (
    "fn main() {\n"
    '    println!("goal-latch h5.2 writer");\n'
    "}\n"
)
NEW_SHA256 = "4cc19279201981c3204ca03a0f243922b3974eddd14313f1f9ad94c11101a370"

OBJECTIVE = (
    "Replace src/main.rs with exactly these UTF-8 bytes, using LF line endings:\n"
    + NEW_CONTENT
    + "The SHA-256 of those bytes is "
    + NEW_SHA256
    + ". Use exactly one CODEX_WRITER task. The task scope must be "
    "allowed_paths ['src/main.rs'], forbidden_paths [], operation_kind "
    "LOCAL_MUTATION, replay_safety VERIFY_BEFORE_RETRY. Use FILE_DIGEST "
    "verification for src/main.rs with expected_sha256 "
    + NEW_SHA256
    + ". The Writer must submit exactly one WRITE_UTF8 operation for src/main.rs "
    "with that exact content and the correct preimage, and its summary must state "
    "that src/main.rs now prints exactly 'goal-latch h5.2 writer'. Do not modify "
    "any other file. Read src/main.rs before proposing the change."
)


def write_fixture_marker(root: Path, state: Path, repo: Path) -> str:
    """Write the H5-C disposable marker with the same shape the Rust reader checks.

    The Rust writer is test-only, so the out-of-process harness writes the
    documented record itself. The reader still enforces: absolute/canonical
    state root, not the default root, disposable parent without `.git`, and a
    workspace sibling under that same parent.
    """
    fixture_id = str(uuid.uuid4())
    record = {
        "fixture_id": fixture_id,
        "state_root": str(state.resolve()),
        "workspace_root": str(repo.resolve()),
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "purpose": "H5.2-B isolated Writer grant fixture; never an Owner approval",
    }
    (state / "test-grant-fixture.json").write_text(
        json.dumps(record, indent=2), encoding="utf-8"
    )
    return fixture_id


def issue_fixture_grant(binary: Path, state: Path, repo: Path, session_id: str, goal_id: str) -> str:
    env = dict(os.environ)
    env["LOCAL_MCP_STATE_DIR"] = str(state)
    completed = subprocess.run(
        [str(binary), "test-grant", session_id, goal_id],
        cwd=str(repo),
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=60,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"test-grant failed ({completed.returncode}): {completed.stderr.strip()}"
        )
    return completed.stdout.strip()


def changed_paths(before: dict[str, str], after: dict[str, str]) -> list[str]:
    return sorted(
        path
        for path in set(before) | set(after)
        if before.get(path) != after.get(path)
    )


def drive(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    assert binary.is_file(), f"binary not found: {binary}"
    binary_sha = hashlib.sha256(binary.read_bytes()).hexdigest()

    fixture = base.make_fixture(
        Path(args.work_dir) if args.work_dir else None, args.run_id
    )
    root, state, repo, session_id = (
        fixture.root,
        fixture.state,
        fixture.workspace,
        fixture.session_id,
    )
    fixture_id = write_fixture_marker(root, state, repo)
    log_dir = fixture.logs
    log_dir.mkdir(parents=True, exist_ok=True)
    evidence: dict = {
        "scenario": "H5.2-B Writer/Reviewer (real model, isolated test grant)",
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "run_id": fixture.run_id,
        "fixture_root": str(root),
        "attempt_dir": str(fixture.attempt_dir),
        "binary": str(binary),
        "binary_sha256": binary_sha,
        "state_root": str(state),
        "workspace": str(repo),
        "session_id": session_id,
        "fixture_id": fixture_id,
        "write_scope": WRITE_SCOPE,
        "steps": [],
        "workspace_before": base.workspace_digest(repo),
    }
    before = dict(evidence["workspace_before"])

    server = base.McpServer(binary, state, repo)
    try:
        view = server.call(
            "goal_start",
            {
                "session_id": session_id,
                "objective": OBJECTIVE,
                "model_backend": "EXTERNAL",
                "write_scope": WRITE_SCOPE,
            },
        )
        goal_id = view["goal_id"]
        evidence["goal_id"] = goal_id
        evidence["goal_start"] = view

        # The stored write_scope is available immediately after goal_start, and
        # the Planner step is refused with WRITE_AUTHORIZATION_UNAVAILABLE until
        # a usable grant exists. Issue the disposable fixture decision first.
        grant_line = issue_fixture_grant(binary, state, repo, session_id, goal_id)
        evidence["test_grant"] = grant_line
        grant_issued = True

        model_calls = 0
        max_calls = args.max_model_calls
        for step in range(args.max_steps):
            next_view = server.call(
                "goal_next", {"session_id": session_id, "goal_id": goal_id}
            )
            step_record = {
                "step": step,
                "outcome": next_view.get("outcome"),
                "status": next_view.get("status"),
                "revision": next_view.get("revision"),
                "plan_revision": next_view.get("plan_revision"),
                "next_action": next_view.get("next_action"),
                "no_action_reason": next_view.get("no_action_reason"),
                "blockers": next_view.get("blockers"),
                "progress": next_view.get("progress"),
                "model_work_id": (next_view.get("model_work") or {}).get("work_id"),
            }
            evidence["steps"].append(step_record)
            work = next_view.get("model_work")
            if work:
                if model_calls >= max_calls:
                    raise RuntimeError(
                        f"model budget exhausted before {work.get('role')}"
                    )
                contract = work["response_contract"]
                step_record["role"] = work["role"]
                step_record["contract_id"] = contract["contract_id"]
                step_record["request"] = work["request"]
                answer, meta = base.ask_model(
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
                receipt = server.call("goal_submit", submission)
                step_record["submission"] = {
                    "submission_id": submission["submission_id"],
                    "outcome": receipt.get("outcome"),
                    "code": receipt.get("code"),
                    "committed_revision": receipt.get("committed_revision"),
                }
                continue

            if next_view.get("status") in (
                "COMPLETED",
                "FAILED",
                "BLOCKED",
                "CANCELLED",
            ):
                break
            if next_view.get("next_action") == "STOP_AND_RECONCILE":
                break
            time.sleep(0.2)

        final = server.call(
            "goal_status", {"session_id": session_id, "goal_id": goal_id}
        )
        evidence["final"] = final
        if final.get("status") == "COMPLETED":
            evidence["goal_result"] = server.call(
                "goal_result", {"session_id": session_id, "goal_id": goal_id}
            )
        evidence["workspace_after"] = base.workspace_digest(repo)
        evidence["changed_paths"] = changed_paths(before, evidence["workspace_after"])
        evidence["model_call_count"] = model_calls
        evidence["grant_issued"] = grant_issued
        evidence["main_rs"] = (repo / "src" / "main.rs").read_text(encoding="utf-8")
        evidence["codex_spawns_for_goal"] = 0
    finally:
        server.close()
        evidence["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        fixture.evidence_path.write_text(
            json.dumps(evidence, indent=2), encoding="utf-8"
        )

    print(
        json.dumps(
            {
                "evidence": str(fixture.evidence_path),
                "goal_id": evidence.get("goal_id"),
                "final_status": (evidence.get("final") or {}).get("status"),
                "model_calls": evidence.get("model_call_count"),
                "grant": evidence.get("test_grant"),
                "changed_paths": evidence.get("changed_paths"),
                "main_rs": evidence.get("main_rs"),
            },
            indent=2,
        )
    )
    ok = (
        (evidence.get("final") or {}).get("status") == "COMPLETED"
        and evidence.get("grant_issued") is True
        and evidence.get("changed_paths") == ["src/main.rs"]
        and "goal-latch h5.2 writer" in (evidence.get("main_rs") or "")
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
    parser.add_argument(
        "--readonly-sandbox",
        choices=("read-only", "workspace-write"),
        default="read-only",
        help=(
            "codex --sandbox for model calls. 'read-only' blinds the model on "
            "Windows; 'workspace-write' is a recorded deviation whose no-write "
            "property is proven by a workspace digest check after every call."
        ),
    )
    parser.add_argument("--max-steps", type=int, default=30)
    parser.add_argument("--max-model-calls", type=int, default=3)
    parser.add_argument("--model", default="gpt-6.1-terra")
    return drive(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
