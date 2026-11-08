"""E5-H3 dogfooding: Scenario B (Writer -> Reviewer -> Host Verify), real stdio MCP.

Runs a *separate* GoalLatch process (the H3 build) in an isolated Git repository
and drives the multi-file Writer path through the real MCP transport:

  goal_start(EXTERNAL) -> goal_next(PLANNER) -> goal_submit(plan)
  -> goal_next(WRITER) -> goal_submit(two WRITE_UTF8 operations)
  -> goal_next(host publication of both files) -> goal_next(REVIEWER)
  -> goal_submit(review verdict) -> goal_next(host Task Verifier + Goal Verifier)

Nothing here touches the running `localmcp` session: the session id, the Git
repository and the state directory entry are all new.

The plan/answers below are authored for this run (not host-consumed fixtures):
this script is the transport, and every host decision (scope, preimage,
publication, review gate, verification) stays with GoalLatch.

Usage:
    LOCAL_MCP_REPO_ROOT=<repo> python scripts/e5-h3-dogfood/driver_h3_writer.py
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
_WORK_ROOT = Path(os.environ.get("LOCAL_MCP_E5H3_WORK", Path(tempfile.gettempdir()) / "gl-e5h3-writer"))
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


def sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


ALPHA_BEFORE = "alpha before\n"
BETA_BEFORE = "beta before\n"
ALPHA_AFTER = "alpha after\n"
BETA_AFTER = "beta after\n"


def prepare_repo(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir(parents=True)
    repo.joinpath("alpha.txt").write_bytes(ALPHA_BEFORE.encode("utf-8"))
    repo.joinpath("beta.txt").write_bytes(BETA_BEFORE.encode("utf-8"))
    run(["git", "init", "-q"], cwd=repo)
    run(["git", "add", "."], cwd=repo)
    run(
        [
            "git",
            "-c",
            "user.email=e5h3@example.invalid",
            "-c",
            "user.name=e5h3 dogfood",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
        cwd=repo,
    )
    return repo


def create_session(repo: Path, session_id: str):
    proc = subprocess.run(
        [str(BINARY), "start", session_id],
        cwd=str(repo),
        input="",
        capture_output=True,
        text=True,
        timeout=60,
    )
    if "local-mcp session:" not in (proc.stderr + proc.stdout):
        raise RuntimeError(f"session creation failed: {proc.stderr}")


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


def main() -> int:
    if not BINARY.exists():
        raise RuntimeError(f"binary not found: {BINARY}")
    repo = prepare_repo(WORK)
    session_id = f"e5h3-writer-{uuid.uuid4().hex[:8]}"
    create_session(repo, session_id)
    record = {"session_id": session_id, "repository": str(repo), "steps": []}

    client = McpClient(repo)
    try:
        started = client.call(
            "goal_start",
            {
                "session_id": session_id,
                "objective": "Apply two scoped writer edits and have them reviewed",
                "completion_criteria": ["alpha.txt and beta.txt contain the requested new text"],
                "model_backend": "EXTERNAL",
            },
        )
        assert not started["is_error"], started
        goal_id = started["body"]["goal_id"]
        record["goal_id"] = goal_id
        record["start_revision"] = started["body"]["revision"]

        planner_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        record["planner_role"] = planner_work["role"]
        criterion_ids = [c["criterion_id"] for c in planner_work["request"]["completion_criteria"]]
        plan = {
            "goal_id": planner_work["goal_id"],
            "goal_revision": planner_work["goal_revision"],
            "summary": "edit two files under host scope",
            "tasks": [
                {
                    "proposal_id": "edit-two",
                    "title": "Edit alpha.txt and beta.txt",
                    "objective": "Replace both files with the requested text",
                    "mandatory": True,
                    "worker": "CODEX_WRITER",
                    "dependencies": [],
                    "scope": {
                        "allowed_paths": ["alpha.txt", "beta.txt"],
                        "forbidden_paths": [],
                        "operation_kind": "LOCAL_MUTATION",
                        "replay_safety": "VERIFY_BEFORE_RETRY",
                    },
                    "verification": [
                        {"kind": "FILE_DIGEST", "path": "alpha.txt", "expected_sha256": sha256(ALPHA_AFTER)},
                        {"kind": "FILE_DIGEST", "path": "beta.txt", "expected_sha256": sha256(BETA_AFTER)},
                        {"kind": "REVIEW_GATE", "max_blocking_findings": 0},
                    ],
                }
            ],
            "criterion_bindings": [
                {"criterion_id": cid, "task_refs": ["edit-two"]} for cid in criterion_ids
            ],
        }
        planned = client.call("goal_submit", submit_args(session_id, goal_id, planner_work, plan))
        assert planned["body"]["outcome"] == "COMMITTED", planned
        record["steps"].append(["plan", planned["body"]["outcome"], planned["body"].get("progress")])

        writer_work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
        assert writer_work["role"] == "WRITER", writer_work
        record["writer_work_id"] = writer_work["work_id"]
        record["writer_attempt_id"] = writer_work["request"]["attempt_id"]
        writer_answer = {
            "goal_id": writer_work["request"]["goal_id"],
            "task_id": writer_work["request"]["task_id"],
            "attempt_id": writer_work["request"]["attempt_id"],
            "goal_revision": writer_work["request"]["goal_revision"],
            "plan_revision": writer_work["request"]["plan_revision"],
            "status": "candidate_complete",
            "summary": "replaced both files",
            "evidence": [{"kind": "writer-evidence", "value": "two files replaced"}],
            "proposed_operations": [
                {
                    "kind": "WRITE_UTF8",
                    "path": "alpha.txt",
                    "expected_preimage": {"kind": "SHA256", "sha256": sha256(ALPHA_BEFORE)},
                    "content": ALPHA_AFTER,
                },
                {
                    "kind": "WRITE_UTF8",
                    "path": "beta.txt",
                    "expected_preimage": {"kind": "SHA256", "sha256": sha256(BETA_BEFORE)},
                    "content": BETA_AFTER,
                },
            ],
        }
        staged = client.call("goal_submit", submit_args(session_id, goal_id, writer_work, writer_answer))
        assert staged["body"]["outcome"] == "ACCEPTED_FOR_HOST_PROCESSING", staged
        record["steps"].append(["writer_submit", staged["body"]["outcome"]])

        # One bounded goal_next publishes both operations and hands back the reviewer.
        reviewed = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        while reviewed["body"].get("outcome") == "HOST_PROGRESS":
            record["steps"].append(["host_progress", reviewed["body"].get("progress", {}).get("code")])
            reviewed = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        assert reviewed["body"]["outcome"] == "MODEL_REQUIRED", reviewed
        assert reviewed["body"]["model_work"]["role"] == "REVIEWER", reviewed
        record["steps"].append(["reviewer_issued", reviewed["body"]["model_work"]["work_id"]])
        record["published_alpha"] = (repo / "alpha.txt").read_text(encoding="utf-8")
        record["published_beta"] = (repo / "beta.txt").read_text(encoding="utf-8")

        reviewer_work = reviewed["body"]["model_work"]
        verdict = client.call(
            "goal_submit",
            submit_args(
                session_id,
                goal_id,
                reviewer_work,
                {
                    "goal_id": reviewer_work["request"]["goal_id"],
                    "task_id": reviewer_work["request"]["task_id"],
                    "attempt_id": reviewer_work["request"]["attempt_id"],
                    "goal_revision": reviewer_work["request"]["goal_revision"],
                    "plan_revision": reviewer_work["request"]["plan_revision"],
                    "summary": "both edits reviewed, no blocking findings",
                    "blocking_findings": 0,
                    "evidence": [{"kind": "review", "value": "diff reviewed"}],
                },
            ),
        )
        assert verdict["body"]["outcome"] == "COMMITTED", verdict
        record["steps"].append(["reviewer_submit", verdict["body"]["outcome"]])

        finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        while finished["body"].get("outcome") == "HOST_PROGRESS":
            record["steps"].append(["host_progress", finished["body"].get("progress", {}).get("code")])
            finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
        record["final_outcome"] = finished["body"].get("outcome")
        record["final_status"] = finished["body"].get("status")

        result = client.call("goal_result", {"session_id": session_id, "goal_id": goal_id})
        record["result"] = result["body"]
        record["git_status"] = run(["git", "status", "--porcelain"], cwd=repo).stdout
        record["git_diff"] = run(["git", "diff", "--stat"], cwd=repo).stdout
        record["codex_processes"] = 0
    finally:
        client.close()

    out = WORK / "h3-writer-dogfood.json"
    out.write_text(json.dumps(record, indent=2, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(record, indent=2, ensure_ascii=False))
    print(f"\nrecord: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
