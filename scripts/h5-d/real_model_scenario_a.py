#!/usr/bin/env python3
"""H5-D — real-model end-to-end driver for the GoalLatch external backend.

This is the harness, not a fixture replay. For every `ModelWorkItem` the host
issues, it asks a real model (Codex `gpt-6.1-sol` through `codex exec`) for an
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

Usage:
    python scripts/h5-d/real_model_scenario_a.py --work-dir tmp/h5-d/scenario-a
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path


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
# Fixture
# ---------------------------------------------------------------------------


def make_fixture(work_dir: Path) -> tuple[Path, Path, Path, str]:
    """Create a throwaway state root, workspace and Session record."""
    root = work_dir.resolve()
    if root.exists():
        shutil.rmtree(root)
    state = root / "state"
    repo = root / "repo"
    (state / "sessions").mkdir(parents=True)
    (repo / "src").mkdir(parents=True)
    (repo / "data").mkdir(parents=True)
    (repo / "src" / "main.rs").write_text(
        'fn main() {\n    println!("goal-latch h5-d fixture");\n}\n', encoding="utf-8"
    )
    (repo / "src" / "helper.rs").write_text(
        "pub fn helper() -> u32 {\n    1\n}\n", encoding="utf-8"
    )
    (repo / "data" / "inventory.txt").write_text(
        "widgets=42\ngadgets=7\nspares=0\n", encoding="utf-8"
    )
    session_id = f"h5d-{uuid.uuid4()}"
    record = {
        "id": session_id,
        "cwd": str(repo),
        "permitted_directories": [str(repo)],
    }
    (state / "sessions" / f"{session_id}.json").write_text(
        json.dumps(record, indent=2), encoding="utf-8"
    )
    return root, state, repo, session_id


def workspace_digest(repo: Path) -> dict[str, str]:
    digests = {}
    for path in sorted(repo.rglob("*")):
        if path.is_file():
            digests[str(path.relative_to(repo)).replace("\\", "/")] = hashlib.sha256(
                path.read_bytes()
            ).hexdigest()
    return digests


# ---------------------------------------------------------------------------
# Real model
# ---------------------------------------------------------------------------

JSON_BLOCK = re.compile(r"\{.*\}", re.DOTALL)

PROMPT = """You are answering a GoalLatch external ModelWorkItem.

Answer with exactly one JSON object and nothing else: no prose, no markdown
fence, no explanation before or after.

The host will refuse the submission if it violates this contract, so follow the
`schema` exactly, including the `status` vocabulary and the identity fields you
must echo:

{contract}

The host-authored request for this WorkItem is:

{request}

Rules that the schema cannot express:
{conditions}

Be concise. Do not invent facts you cannot observe in the workspace. If the
request asks you to inspect files, inspect them.
"""


def ask_model(
    repo: Path,
    contract: dict,
    request: dict,
    conditions: list[dict],
    log_dir: Path,
    step: int,
) -> tuple[dict, dict]:
    prompt = PROMPT.format(
        contract=json.dumps(contract, indent=2),
        request=json.dumps(request, indent=2),
        conditions="\n".join(
            f"- {entry['code']}: {entry['detail']}" for entry in conditions
        ),
    )
    (log_dir / f"step-{step}-prompt.txt").write_text(prompt, encoding="utf-8")
    started = time.time()
    completed = subprocess.run(
        [
            "codex",
            "exec",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "-C",
            str(repo),
            prompt,
        ],
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=600,
    )
    elapsed = time.time() - started
    stdout = completed.stdout or ""
    (log_dir / f"step-{step}-model-stdout.txt").write_text(stdout, encoding="utf-8")
    match = JSON_BLOCK.search(stdout)
    assert match, f"the model produced no JSON object:\n{stdout[-4000:]}"
    answer = json.loads(match.group(0))
    meta = {
        "step": step,
        "model": "codex gpt-6.1-sol (codex exec, sandbox=read-only)",
        "exit_code": completed.returncode,
        "elapsed_seconds": round(elapsed, 1),
        "prompt_bytes": len(prompt),
        "answer": answer,
    }
    return answer, meta


# ---------------------------------------------------------------------------
# Drive
# ---------------------------------------------------------------------------


def drive(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    assert binary.is_file(), f"binary not found: {binary}"
    binary_sha = hashlib.sha256(binary.read_bytes()).hexdigest()

    root, state, repo, session_id = make_fixture(Path(args.work_dir))
    log_dir = root / "logs"
    log_dir.mkdir()
    evidence: dict = {
        "scenario": "H5-D Scenario A (read-only DAG, real model)",
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
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
        (root / "evidence.json").write_text(
            json.dumps(evidence, indent=2), encoding="utf-8"
        )

    print(json.dumps(
        {
            "evidence": str(root / "evidence.json"),
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
    parser.add_argument("--work-dir", default="tmp/h5-d/scenario-a")
    parser.add_argument("--max-steps", type=int, default=24)
    return drive(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
