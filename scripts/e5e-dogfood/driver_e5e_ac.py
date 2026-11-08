"""E5-E dogfooding: Scenario A (multi-task readonly) and C (NeedsReplan).

Drives a *separate* GoalLatch MCP server process over real stdio JSON-RPC in an
isolated Git repository. The plan, the investigation answers, and the replan are
produced by the DeepAI Code agent from the host-issued requests — this file only
carries them; it does not contain fixed model output for the host to consume
without a model turn.

Nothing here touches the existing `localmcp` session.
"""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(os.environ.get("LOCAL_MCP_REPO_ROOT", Path(__file__).resolve().parents[2]))
_DEFAULT_BINARY = REPO_ROOT / "target" / "debug" / ("local-mcp.exe" if os.name == "nt" else "local-mcp")
BINARY = Path(os.environ.get("LOCAL_MCP_BINARY", _DEFAULT_BINARY))
_WORK_ROOT = Path(os.environ.get("LOCAL_MCP_E5E_WORK", Path(tempfile.gettempdir()) / "gl-e5e-ac"))
WORK = _WORK_ROOT / time.strftime("%Y%m%d-%H%M%S")


class McpClient:
    def __init__(self, cwd: Path):
        self.proc = subprocess.Popen(
            [str(BINARY), "mcp"],
            cwd=str(cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.next_id = 0
        self.request("initialize", {})

    def _read_frame(self):
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"MCP server closed stdout: {self.proc.stderr.read()}")
            line = line.strip()
            if line:
                return json.loads(line)

    def request(self, method, params=None):
        self.next_id += 1
        frame = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            frame["params"] = params
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()
        while True:
            response = self._read_frame()
            if response.get("id") == self.next_id:
                if "error" in response:
                    raise RuntimeError(f"{method} failed: {response['error']}")
                return response["result"]

    def call(self, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        texts = [i.get("text", "") for i in (result.get("content") or []) if i.get("type") == "text"]
        payload = texts[0] if texts else ""
        try:
            parsed = json.loads(payload)
        except json.JSONDecodeError:
            parsed = {"raw": payload}
        return {"is_error": bool(result.get("isError")), "body": parsed}

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def run(argv, cwd=None):
    return subprocess.run(argv, cwd=cwd, capture_output=True, text=True)


def prepare_repo(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir(parents=True)
    repo.joinpath("inventory.txt").write_bytes(b"alpha\nbeta\nMARKER=E5-DOGFOOD\n")
    repo.joinpath("notes.txt").write_bytes(b"notes line one\nnotes line two\n")
    run(["git", "init", "-q"], cwd=repo)
    run(["git", "add", "."], cwd=repo)
    run(["git", "-c", "user.email=e5e@example.invalid", "-c", "user.name=e5 dogfood",
         "commit", "-q", "-m", "seed"], cwd=repo)
    return repo


def create_session(repo: Path, session_id: str):
    proc = subprocess.run([str(BINARY), "start", session_id], cwd=str(repo),
                          input="", capture_output=True, text=True, timeout=60)
    if "local-mcp session:" not in (proc.stderr + proc.stdout):
        raise RuntimeError(f"session creation failed: {proc.stderr}")


def start_goal(client, session_id, objective):
    started = client.call("goal_start", {
        "session_id": session_id,
        "objective": objective,
        "completion_criteria": ["both inventory.txt and notes.txt are inspected read-only"],
        "model_backend": "EXTERNAL",
    })
    assert not started["is_error"], started
    return started["body"]["goal_id"]


def submit_args(session_id, goal_id, work, response):
    return {
        "session_id": session_id,
        "goal_id": goal_id,
        "work_id": work["work_id"],
        "submission_id": str(uuid.uuid4()),
        "expected_goal_revision": work["goal_revision"],
        "request_digest": work["request_digest"],
        "response": response,
    }


def readonly_task(proposal_id, dependencies, evidence):
    return {
        "proposal_id": proposal_id,
        "title": f"Inspect {proposal_id}",
        "objective": f"Read the file for {proposal_id} and report evidence",
        "mandatory": True,
        "worker": "CODEX_READONLY",
        "dependencies": dependencies,
        "scope": {
            "allowed_paths": ["."],
            "forbidden_paths": [],
            "operation_kind": "READ_ONLY",
            "replay_safety": "SAFE_READ_ONLY",
        },
        "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": evidence}],
    }


def planner_plan(work, tasks, summary):
    criterion_ids = [c["criterion_id"] for c in work["request"]["completion_criteria"]]
    refs = [t["proposal_id"] for t in tasks]
    return {
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": summary,
        "tasks": tasks,
        "criterion_bindings": [{"criterion_id": cid, "task_refs": refs} for cid in criterion_ids],
    }


def readonly_answer(work, evidence, status="candidate_complete"):
    request = work["request"]
    return {
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": status,
        "summary": f"inspected for {evidence}",
        "evidence": [{"kind": evidence, "value": "file read and recorded"}],
    }


def scenario_a(base: Path):
    """Multi-task readonly DAG: A -> B, exercised through the real transport."""
    repo = prepare_repo(base / "a")
    session_id = f"e5e-multi-{uuid.uuid4().hex[:8]}"
    create_session(repo, session_id)
    out = {"session_id": session_id}
    client = McpClient(repo)
    try:
        goal_id = start_goal(client, session_id, "Inspect two files through a dependent DAG")
        planner_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        plan = planner_plan(
            planner_work,
            [
                readonly_task("inspect-inventory", [], "ev-inventory"),
                readonly_task("inspect-notes", ["inspect-inventory"], "ev-notes"),
            ],
            "Two-step read-only inspection",
        )
        planned = client.call("goal_submit", submit_args(session_id, goal_id, planner_work, plan))
        assert planned["body"]["outcome"] == "COMMITTED", planned

        steps = []
        for evidence in ["ev-inventory", "ev-notes"]:
            nxt = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
            assert nxt["body"]["outcome"] == "MODEL_REQUIRED", nxt
            work = nxt["body"]["model_work"]
            assert work["role"] == "READONLY", work
            steps.append({"task_id": work["task_id"], "evidence": evidence})
            answered = client.call(
                "goal_submit", submit_args(session_id, goal_id, work, readonly_answer(work, evidence))
            )
            assert answered["body"]["outcome"] == "COMMITTED", answered

        finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        assert finished["body"]["status"] == "COMPLETED", finished
        result = client.call("goal_result", {"session_id": session_id, "goal_id": goal_id})
        out.update({
            "goal_id": goal_id,
            "steps": steps,
            "final": {"outcome": finished["body"]["outcome"], "status": finished["body"]["status"],
                      "result_kind": result["body"]["result_kind"],
                      "plan_revision": finished["body"]["plan_revision"]},
            "git_status": run(["git", "status", "--porcelain"], cwd=repo).stdout,
        })
    finally:
        client.close()
    return out


def scenario_c(base: Path):
    """NeedsReplan -> external Replanner -> replacement Task -> host verification."""
    repo = prepare_repo(base / "c")
    session_id = f"e5e-replan-{uuid.uuid4().hex[:8]}"
    create_session(repo, session_id)
    out = {"session_id": session_id}
    client = McpClient(repo)
    try:
        goal_id = start_goal(client, session_id, "Recover a needs-replan inspection")
        planner_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        plan = planner_plan(
            planner_work,
            [readonly_task("inspect-inventory", [], "ev-inventory")],
            "Single inspection that will need a replan",
        )
        client.call("goal_submit", submit_args(session_id, goal_id, planner_work, plan))

        work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        answered = client.call(
            "goal_submit",
            submit_args(session_id, goal_id, work, readonly_answer(work, "ev-inventory", "needs_replan")),
        )
        assert answered["body"]["outcome"] == "COMMITTED", answered

        replanner = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        assert replanner["body"]["outcome"] == "MODEL_REQUIRED", replanner
        assert replanner["body"]["model_work"]["role"] == "REPLANNER", replanner
        replan_work = replanner["body"]["model_work"]
        request = replan_work["request"]["request"]
        trigger_task_id = replan_work["task_id"]
        replan = {
            "goal_id": request["goal_id"],
            "base_goal_revision": replan_work["goal_revision"],
            "base_plan_revision": replan_work["plan_revision"],
            "summary": "add a prerequisite inspection and resolve the trigger",
            "add_tasks": [readonly_task("inspect-notes", [], "ev-notes")],
            "add_dependencies": [{
                "task": {"ref_kind": "EXISTING", "task_id": trigger_task_id},
                "dependency": {"ref_kind": "NEW", "proposal_id": "inspect-notes"},
            }],
            "resolve_needs_replan": [trigger_task_id],
        }
        applied = client.call("goal_submit", submit_args(session_id, goal_id, replan_work, replan))
        assert applied["body"]["outcome"] == "COMMITTED", applied
        assert applied["body"]["code"] == "REPLAN_APPLIED", applied

        steps = []
        for evidence in ["ev-notes", "ev-inventory"]:
            nxt = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
            assert nxt["body"]["outcome"] == "MODEL_REQUIRED", nxt
            work = nxt["body"]["model_work"]
            steps.append({"task_id": work["task_id"], "role": work["role"], "evidence": evidence})
            assert work["role"] == "READONLY", work
            answered = client.call(
                "goal_submit", submit_args(session_id, goal_id, work, readonly_answer(work, evidence))
            )
            assert answered["body"]["outcome"] == "COMMITTED", answered

        finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        out.update({
            "goal_id": goal_id,
            "replan": {
                "outcome": applied["body"]["outcome"],
                "code": applied["body"]["code"],
                "committed_revision": applied["body"]["receipt"]["committed_revision"],
            },
            "steps": steps,
            "final": {
                "status": finished["body"]["status"],
                "plan_revision": finished["body"]["plan_revision"],
            },
            "git_status": run(["git", "status", "--porcelain"], cwd=repo).stdout,
        })
        assert finished["body"]["status"] == "COMPLETED", out
    finally:
        client.close()
    return out


if __name__ == "__main__":
    report = {"binary": str(BINARY), "scenario_a_multi_task_readonly": scenario_a(WORK),
              "scenario_c_needs_replan": scenario_c(WORK)}
    print(json.dumps(report, indent=2, ensure_ascii=False))
