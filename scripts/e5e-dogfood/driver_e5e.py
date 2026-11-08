"""E5-E dogfooding driver: External Writer + Reviewer, and crash/recovery.

Drives a *separate* GoalLatch MCP server process (built from this branch) over
real stdio JSON-RPC in an isolated Git repository, exactly like the E4-C driver.
Nothing here touches the user's existing `localmcp` session.

Scenario B: one scoped WRITE_UTF8 change proposed by the agent, validated and
staged by the host, published through the host write boundary, reviewed, then
verified by the host Task/Goal verifier.

Scenario D: process interruption after the Writer submission (staged, not
published) and after publication, then reconnect. The staged bytes must be
reused (no re-ask, no double write) and exactly one publish must be observable.
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
_WORK_ROOT = Path(os.environ.get("LOCAL_MCP_E5E_WORK", Path(tempfile.gettempdir()) / "gl-e5e-dogfood"))
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
    (repo / "README.md").write_text("# e5 writer dogfood\n", encoding="utf-8")
    (repo / "greeting.txt").write_text("hello from goal-latch\n", encoding="utf-8")
    run(["git", "init", "-q"], cwd=repo)
    run(["git", "add", "."], cwd=repo)
    run(
        ["git", "-c", "user.email=e5e@example.invalid", "-c", "user.name=e5 dogfood",
         "commit", "-q", "-m", "seed"],
        cwd=repo,
    )
    return repo


def create_session(repo: Path, session_id: str):
    proc = subprocess.run(
        [str(BINARY), "start", session_id],
        cwd=str(repo), input="", capture_output=True, text=True, timeout=60,
    )
    if "local-mcp session:" not in (proc.stderr + proc.stdout):
        raise RuntimeError(f"session creation failed: {proc.stderr}")


def sha256_text(text: str) -> str:
    """Digest of the exact bytes stored for a UTF-8 string."""
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def start_goal(client, session_id, objective):
    started = client.call(
        "goal_start",
        {
            "session_id": session_id,
            "objective": objective,
            "completion_criteria": [
                "greeting.txt contains the requested reviewed change",
            ],
            "model_backend": "EXTERNAL",
        },
    )
    assert not started["is_error"], started
    return started["body"]["goal_id"]


def writer_plan(work, new_content):
    criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
    return {
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "Apply one reviewed scoped edit",
        "tasks": [{
            "proposal_id": "edit-greeting",
            "title": "Update greeting.txt",
            "objective": "Replace the greeting with the requested reviewed content",
            "mandatory": True,
            "worker": "CODEX_WRITER",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["greeting.txt"],
                "forbidden_paths": [],
                "operation_kind": "LOCAL_MUTATION",
                "replay_safety": "VERIFY_BEFORE_RETRY",
            },
            "verification": [
                {"kind": "FILE_DIGEST", "path": "greeting.txt", "expected_sha256": sha256_text(new_content)},
                {"kind": "REVIEW_GATE", "max_blocking_findings": 0},
            ],
        }],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["edit-greeting"]}],
    }


def writer_answer(work, new_content, previous_content):
    request = work["request"]
    return {
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "replace the greeting with the reviewed content",
        "evidence": [{"kind": "writer-evidence", "value": "one scoped file replaced"}],
        "proposed_operations": [{
            "kind": "WRITE_UTF8",
            "path": "greeting.txt",
            "expected_preimage": {"kind": "SHA256", "sha256": sha256_text(previous_content)},
            "content": new_content,
        }],
    }


def reviewer_answer(work):
    request = work["request"]
    return {
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "summary": "the applied change matches the requested greeting",
        "blocking_findings": 0,
        "evidence": [{"kind": "review", "value": "content reviewed against the task objective"}],
    }


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


def scenario_b(base: Path):
    repo = prepare_repo(base / "b")
    session_id = f"e5e-writer-{uuid.uuid4().hex[:8]}"
    create_session(repo, session_id)
    previous = (repo / "greeting.txt").read_bytes().decode("utf-8")
    new_content = "hello from goal-latch, written by an external agent\n"
    out = {"session_id": session_id, "repo": str(repo)}

    client = McpClient(repo)
    try:
        goal_id = start_goal(client, session_id, "Apply one reviewed scoped greeting change")
        planner_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        planned = client.call("goal_submit", submit_args(session_id, goal_id, planner_work, writer_plan(planner_work, new_content)))
        assert planned["body"]["outcome"] == "COMMITTED", planned

        writer_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        assert writer_work["role"] == "WRITER", writer_work
        staged = client.call("goal_submit", submit_args(session_id, goal_id, writer_work, writer_answer(writer_work, new_content, previous)))
        assert staged["body"]["outcome"] == "ACCEPTED_FOR_HOST_PROCESSING", staged
        assert staged["body"]["code"] == "WRITE_INTENT_PREPARED", staged

        reviewed = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        assert reviewed["body"]["outcome"] == "MODEL_REQUIRED", reviewed
        reviewer_work = reviewed["body"]["model_work"]
        assert reviewer_work["role"] == "REVIEWER", reviewer_work
        published_content = (repo / "greeting.txt").read_bytes().decode("utf-8")
        assert published_content == new_content, "the host published exactly the staged bytes"

        verdict = client.call("goal_submit", submit_args(session_id, goal_id, reviewer_work, reviewer_answer(reviewer_work)))
        assert verdict["body"]["outcome"] == "COMMITTED", verdict

        finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        assert finished["body"]["status"] == "COMPLETED", finished
        result = client.call("goal_result", {"session_id": session_id, "goal_id": goal_id})

        out.update({
            "goal_id": goal_id,
            "planner": planned["body"]["outcome"],
            "writer_submission": {"outcome": staged["body"]["outcome"], "code": staged["body"]["code"]},
            "publication_step": {"outcome": reviewed["body"]["outcome"], "role": reviewer_work["role"]},
            "reviewer": {"outcome": verdict["body"]["outcome"], "code": verdict["body"]["code"]},
            "final": {"outcome": finished["body"]["outcome"], "status": finished["body"]["status"],
                      "result_kind": result["body"]["result_kind"]},
            "content_after": (repo / "greeting.txt").read_bytes().decode("utf-8"),
            "content_digest": hashlib.sha256((repo / "greeting.txt").read_bytes()).hexdigest(),
            "git_status": run(["git", "status", "--porcelain"], cwd=repo).stdout,
            "git_diff": run(["git", "diff", "--stat"], cwd=repo).stdout,
        })
    finally:
        client.close()
    return out


def scenario_d(base: Path):
    """Crash after staging (before publication), then after publication."""
    repo = prepare_repo(base / "d")
    session_id = f"e5e-crash-{uuid.uuid4().hex[:8]}"
    create_session(repo, session_id)
    previous = (repo / "greeting.txt").read_bytes().decode("utf-8")
    new_content = "hello from goal-latch, recovered after a disconnect\n"
    out = {"session_id": session_id}

    client = McpClient(repo)
    goal_id = start_goal(client, session_id, "Crash recovery across publication")
    planner_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
    client.call("goal_submit", submit_args(session_id, goal_id, planner_work, writer_plan(planner_work, new_content)))
    writer_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
    staged = client.call("goal_submit", submit_args(session_id, goal_id, writer_work, writer_answer(writer_work, new_content, previous)))
    assert staged["body"]["outcome"] == "ACCEPTED_FOR_HOST_PROCESSING", staged
    # Crash BEFORE publication: the file must be untouched and the stage durable.
    client.close()
    untouched = (repo / "greeting.txt").read_bytes().decode("utf-8")
    out["before_publication_disconnect"] = {
        "content_unchanged": untouched == previous,
        "work_id": writer_work["work_id"],
        "request_digest": writer_work["request_digest"],
    }

    client = McpClient(repo)
    resumed = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    assert resumed["body"]["outcome"] == "MODEL_REQUIRED", resumed
    assert resumed["body"]["model_work"]["role"] == "REVIEWER", resumed
    published_once = (repo / "greeting.txt").read_bytes().decode("utf-8")
    out["resumed_publication"] = {
        "content_is_staged_bytes": published_once == new_content,
        "reviewer_role": resumed["body"]["model_work"]["role"],
    }
    # Crash again AFTER publication, before the verdict.
    client.close()
    after_crash = (repo / "greeting.txt").read_bytes().decode("utf-8")

    client = McpClient(repo)
    again = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    out["after_publication_disconnect"] = {
        "content_stable": (repo / "greeting.txt").read_bytes().decode("utf-8") == after_crash,
        "outcome": again["body"]["outcome"],
        "role": (again["body"]["model_work"] or {}).get("role"),
        "same_work_id": (again["body"]["model_work"] or {}).get("work_id"),
    }
    if again["body"]["outcome"] == "MODEL_REQUIRED":
        verdict = client.call("goal_submit", submit_args(session_id, goal_id, again["body"]["model_work"], reviewer_answer(again["body"]["model_work"])))
        out["verdict_after_recovery"] = verdict["body"]["outcome"]
        finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        out["final"] = {"status": finished["body"]["status"]}
    client.close()
    out["git_status"] = run(["git", "status", "--porcelain"], cwd=repo).stdout
    return out


if __name__ == "__main__":
    report = {
        "binary": str(BINARY),
        "scenario_b_writer_and_review": scenario_b(WORK),
        "scenario_d_crash_recovery": scenario_d(WORK),
    }
    print(json.dumps(report, indent=2, ensure_ascii=False))
