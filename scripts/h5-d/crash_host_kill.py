#!/usr/bin/env python3
"""H5.3-C — real-process crash and recovery harness for Writer publication.

This is a **fault-injection harness**, not a model test. Every model answer it
submits is a fixed, host-contract-shaped object authored here; the thing under
test is what the *host processes* do when the host is killed mid-publication.

Two modes, deliberately separated, because they prove different things:

``--mode d1``  Normal Windows containment.
    The host starts a real Writer publication, the real ``atomic-publish``
    helper enters its critical section (it holds the OS publication lock and has
    written nothing yet), and the host is killed with ``TerminateProcess``. The
    Windows Job Object carries ``KILL_ON_JOB_CLOSE``, so the helper must die with
    the host. A fresh host then has to prove quiescence, refuse to blind-retry an
    UNKNOWN side effect, and never report the Goal complete.

``--mode d2``  A surviving writer-capable process.
    The host is killed mid-publication, and *after* the helper is gone a
    test-owned stand-in process takes the same OS lock and stays alive. A fresh
    host must then observe ``Active`` quiescence, keep the publication
    quarantine, refuse to confirm a revocation, and refuse to complete the Goal
    until the survivor is gone and quiescence is proven.

What D2 does **not** prove: the stand-in is not a detached ``atomic-publish``
helper. It proves the host's recovery decision procedure treats a live
lock-holder as unresolved. It says nothing about whether the shipped helper can
escape its Job Object, because it cannot and this harness does not try to make
it.

Both modes only ever touch a disposable fixture root created by
``real_model_scenario_a.make_fixture``, which never deletes anything.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import real_model_scenario_a as base  # noqa: E402
import real_model_scenario_b as grants  # noqa: E402


TARGET = "src/main.rs"
NEW_CONTENT = 'fn main() {\n    println!("goal-latch h5.3 crash survivor");\n}\n'
NEW_SHA256 = hashlib.sha256(NEW_CONTENT.encode("utf-8")).hexdigest()

WRITE_SCOPE = {
    "allowed_paths": [TARGET],
    "allowed_operations": ["WRITE_UTF8"],
    "max_operations": 1,
    "max_bytes": 4096,
    "expires_in_seconds": 900,
}

OBJECTIVE = (
    "Replace src/main.rs with exactly the UTF-8 content the plan describes, "
    "using LF line endings, in one bounded CODEX_WRITER Task. The Task scope is "
    "allowed_paths ['src/main.rs'], forbidden_paths [], operation_kind "
    "LOCAL_MUTATION, replay_safety VERIFY_BEFORE_RETRY. Use FILE_DIGEST "
    "verification for src/main.rs."
)

HOLD_POINT = "in-critical-section"


# ---------------------------------------------------------------------------
# MCP transport with an optional held request
# ---------------------------------------------------------------------------


class CrashServer(base.McpServer):
    """An MCP server whose environment carries the helper hold directory."""

    def __init__(self, binary: Path, state: Path, cwd: Path, extra_env: dict):
        self.hold_dir = extra_env.pop("LOCAL_MCP_TEST_HELPER_HOLD", None)
        self._extra_env = extra_env
        # `base.McpServer` builds its own environment; replicate it with extras.
        import io  # noqa: F401  (kept for parity with the parent implementation)

        env = dict(os.environ)
        env["LOCAL_MCP_STATE_DIR"] = str(state)
        env.update(extra_env)
        if self.hold_dir:
            env["LOCAL_MCP_TEST_HELPER_HOLD"] = str(self.hold_dir)
        self.log = []
        self.child = subprocess.Popen(
            [str(binary), "mcp"],
            cwd=str(cwd),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self._id = 0
        initialized = self.request("initialize", {"protocolVersion": "2025-06-18"})
        name = initialized["result"]["serverInfo"]["name"]
        assert name == "local-mcp", f"unexpected server identity {name!r}"

    def send_only(self, name: str, arguments: dict) -> int:
        """Write a tools/call request and deliberately do not read the reply."""
        self._id += 1
        request_id = self._id
        line = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }
        )
        self.child.stdin.write(line + "\n")
        self.child.stdin.flush()
        return request_id

    def kill_hard(self) -> None:
        """`TerminateProcess`, i.e. a host crash with no unwinding."""
        subprocess.run(
            ["taskkill", "/F", "/PID", str(self.child.pid)],
            capture_output=True,
            text=True,
        )
        try:
            self.child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.child.kill()

    def stderr_tail(self) -> str:
        try:
            return (self.child.stderr.read() or "")[-2000:]
        except (OSError, ValueError):
            return ""


# ---------------------------------------------------------------------------
# Deterministic host-contract answers (no model)
# ---------------------------------------------------------------------------


def planner_answer(work: dict) -> dict:
    request = work["request"]
    criteria = request.get("completion_criteria") or []
    assert criteria, "the host must publish completion criteria for a Planner request"
    criterion_id = criteria[0]["criterion_id"]
    return {
        "goal_id": request["goal_id"],
        "goal_revision": request["goal_revision"],
        "summary": "one bounded writer task with a file-digest verification",
        "tasks": [
            {
                "proposal_id": "write-main",
                "title": "Publish the crash-harness main.rs",
                "objective": (
                    "Replace src/main.rs with exactly the requested UTF-8 content."
                ),
                "mandatory": True,
                "worker": "CODEX_WRITER",
                "scope": {
                    "allowed_paths": [TARGET],
                    "forbidden_paths": [],
                    "operation_kind": "LOCAL_MUTATION",
                    "replay_safety": "VERIFY_BEFORE_RETRY",
                },
                "verification": [
                    {"kind": "FILE_DIGEST", "path": TARGET, "expected_sha256": NEW_SHA256}
                ],
            }
        ],
        "criterion_bindings": [
            {"criterion_id": criterion_id, "task_refs": ["write-main"]}
        ],
    }


def writer_answer(work: dict, preimage_sha256: str | None) -> dict:
    request = work["request"]
    preimage = (
        {"kind": "ABSENT"}
        if preimage_sha256 is None
        else {"kind": "SHA256", "sha256": preimage_sha256}
    )
    return {
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "src/main.rs now prints exactly the crash-harness banner",
        "evidence": [{"kind": "INSPECTION", "value": "read src/main.rs"}],
        "proposed_operations": [
            {
                "kind": "WRITE_UTF8",
                "path": TARGET,
                "expected_preimage": preimage,
                "content": NEW_CONTENT,
            }
        ],
    }


# ---------------------------------------------------------------------------
# Survivor process (D2)
# ---------------------------------------------------------------------------


SURVIVOR_SOURCE = r"""
import msvcrt, os, sys, time
lock_path, ready_path, release_path = sys.argv[1], sys.argv[2], sys.argv[3]
fd = os.open(lock_path, os.O_RDWR)
msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
open(ready_path, "w").write(str(os.getpid()))
deadline = time.time() + 300
while time.time() < deadline and not os.path.exists(release_path):
    time.sleep(0.05)
msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
os.close(fd)
"""


def start_survivor(lock_path: Path, ready: Path, release: Path) -> subprocess.Popen:
    child = subprocess.Popen(
        [sys.executable, "-c", SURVIVOR_SOURCE, str(lock_path), str(ready), str(release)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    deadline = time.time() + 30
    while time.time() < deadline:
        if ready.is_file() and ready.read_text(encoding="utf-8").strip():
            return child
        time.sleep(0.05)
    child.kill()
    raise RuntimeError("the test lock holder never acquired the publication lock")


def process_alive(pid: int) -> bool:
    completed = subprocess.run(
        ["tasklist", "/FI", f"PID eq {pid}", "/NH"],
        capture_output=True,
        text=True,
    )
    return str(pid) in (completed.stdout or "")


def wait_for_death(pid: int, seconds: float) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline:
        if not process_alive(pid):
            return True
        time.sleep(0.05)
    return not process_alive(pid)


def wait_for_file(path: Path, seconds: float) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline:
        if path.is_file():
            return True
        time.sleep(0.02)
    return path.is_file()


def durable_quarantine(state: Path) -> dict:
    """The quarantine as it is durably recorded, not as a view projects it."""
    goals = [
        path
        for path in (state / "goals").glob("*/*.json")
        if not path.name.endswith(".lock")
    ]
    record = {}
    for path in goals:
        goal = json.loads(path.read_text(encoding="utf-8"))
        record[str(path)] = {
            "status": goal.get("status"),
            "quarantine": goal.get("publication_quarantine"),
            "tasks": [
                {
                    "status": task.get("status"),
                    "side_effect_state": (
                        task.get("attempts") or [{}]
                    )[-1].get("side_effect_state"),
                    "intent_operations": [
                        (op.get("index"), op.get("state"))
                        for op in (
                            ((task.get("attempts") or [{}])[-1].get("mutation_intent") or {})
                            .get("operations")
                            or []
                        )
                    ],
                }
                for task in goal.get("tasks") or []
            ],
        }
    return record


def lock_contents(lock_path: Path) -> str:
    try:
        return lock_path.read_text(encoding="utf-8").strip()
    except OSError as error:
        return f"UNREADABLE: {error}"


def read_hold(path: Path) -> dict:
    fields = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            fields[key.strip()] = value.strip()
    return fields


# ---------------------------------------------------------------------------
# Drive
# ---------------------------------------------------------------------------


def drive(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    assert binary.is_file(), f"binary not found: {binary}"
    mode = args.mode

    fixture = base.make_fixture(
        Path(args.work_dir) if args.work_dir else None, args.run_id
    )
    root, state, repo, session_id = (
        fixture.root,
        fixture.state,
        fixture.workspace,
        fixture.session_id,
    )
    hold_dir = fixture.attempt_dir / "hold"
    hold_dir.mkdir(parents=True, exist_ok=True)
    reached = hold_dir / f"{HOLD_POINT}.reached"
    release = hold_dir / f"{HOLD_POINT}.release"

    grants.write_fixture_marker(root, state, repo)
    target = repo / TARGET
    preimage = hashlib.sha256(target.read_bytes()).hexdigest() if target.is_file() else None

    evidence: dict = {
        "scenario": f"H5.3-C {mode.upper()} (real processes, deterministic answers)",
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "run_id": fixture.run_id,
        "mode": mode,
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "state_root": str(state),
        "workspace": str(repo),
        "session_id": session_id,
        "target": TARGET,
        "preimage_sha256": preimage,
        "postimage_sha256": NEW_SHA256,
        "host_fault_injection": "TerminateProcess on the host during publication",
        "model_inference": "none (deterministic host-contract answers)",
        "steps": [],
    }

    server = CrashServer(
        binary, state, repo, {"LOCAL_MCP_TEST_HELPER_HOLD": str(hold_dir)}
    )
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
        evidence["goal_start_status"] = view.get("status")
        evidence["test_grant"] = grants.issue_fixture_grant(
            binary, state, repo, session_id, goal_id
        )

        next_view = server.call(
            "goal_next", {"session_id": session_id, "goal_id": goal_id}
        )
        work = next_view.get("model_work")
        assert work and work["role"] == "PLANNER", f"expected a Planner WorkItem: {next_view}"
        plan = planner_answer(work)
        receipt = server.call(
            "goal_submit",
            {
                "session_id": session_id,
                "goal_id": goal_id,
                "work_id": work["work_id"],
                "submission_id": str(uuid.uuid4()),
                "expected_goal_revision": work["goal_revision"],
                "request_digest": work["request_digest"],
                "response": plan,
            },
        )
        evidence["planner_receipt"] = {
            "outcome": receipt.get("outcome"),
            "code": receipt.get("code"),
        }
        assert receipt.get("code") == "PLAN_APPLIED", receipt

        next_view = server.call(
            "goal_next", {"session_id": session_id, "goal_id": goal_id}
        )
        work = next_view.get("model_work")
        assert work and work["role"] == "WRITER", f"expected a Writer WorkItem: {next_view}"
        answer = writer_answer(work, preimage)
        submission = {
            "session_id": session_id,
            "goal_id": goal_id,
            "work_id": work["work_id"],
            "submission_id": str(uuid.uuid4()),
            "expected_goal_revision": work["goal_revision"],
            "request_digest": work["request_digest"],
            "response": answer,
        }
        evidence["writer_submission_id"] = submission["submission_id"]
        writer_receipt = server.call("goal_submit", submission)
        evidence["writer_receipt"] = {
            "outcome": writer_receipt.get("outcome"),
            "code": writer_receipt.get("code"),
        }
        # Publication is a scheduler step, not part of the writer submission: the
        # host commits the Writer result first and arms/publishes on a later
        # `goal_next`, which is exactly the step this harness must crash inside.
        server.send_only(
            "goal_next",
            {"session_id": session_id, "goal_id": goal_id, "max_host_steps": 1},
        )

        if not wait_for_file(reached, args.hold_timeout):
            evidence["crash"] = {
                "result": "HELPER_NEVER_REACHED_CRITICAL_SECTION",
                "host_stderr_tail": server.stderr_tail(),
            }
            raise RuntimeError(
                "atomic-publish never reached the injected critical-section hold; "
                "the crash was not injected and nothing is proven"
            )
        hold = read_hold(reached)
        helper_pid = int(hold["pid"])
        evidence["critical_section"] = {
            "reached": True,
            "helper_pid": helper_pid,
            "publication_lock": hold.get("lock"),
            "postimage_absent_at_crash": target.read_bytes() != NEW_CONTENT.encode("utf-8"),
        }

        if mode == "d2":
            survivor_ready = hold_dir / "survivor.ready"
            survivor_release = hold_dir / "survivor.release"
            evidence["survivor"] = {"kind": "test-only OS-lock holder"}

        evidence.setdefault("crash", {})
        server.kill_hard()
        host_pid = server.child.pid
        evidence["crash"]["host_pid"] = host_pid
        evidence["crash"]["host_exit_code"] = server.child.returncode
        evidence["crash"]["helper_died_with_host"] = wait_for_death(helper_pid, 30)
        evidence["crash"]["helper_pid"] = helper_pid

        survivor = None
        if mode == "d2":
            lock_path = Path(hold["lock"])
            evidence["survivor"]["lock_path"] = str(lock_path)
            evidence["survivor"]["helper_gone_before_start"] = not process_alive(
                helper_pid
            )
            survivor = start_survivor(lock_path, survivor_ready, survivor_release)
            evidence["survivor"]["pid"] = survivor.pid
            evidence["survivor"]["alive_after_host_death"] = process_alive(survivor.pid)
            (hold_dir / f"{HOLD_POINT}.release").write_text("go\n", encoding="utf-8")
    finally:
        try:
            if server.child.poll() is None:
                server.child.terminate()
                server.child.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            pass

    # ---- recovery: a brand new host process over the same durable state ----
    recovered = CrashServer(binary, state, repo, {})
    try:
        status = recovered.call(
            "goal_status", {"session_id": session_id, "goal_id": evidence["goal_id"]}
        )
        evidence["after_restart"] = {"status": status.get("status"), "revision": status.get("revision")}
        resume = recovered.call(
            "goal_resume", {"session_id": session_id, "goal_id": evidence["goal_id"]}
        )
        evidence["goal_resume"] = {
            "status": resume.get("status"),
            "revision": resume.get("revision"),
            "plan_revision": resume.get("plan_revision"),
            "quarantine": resume.get("publication_quarantine"),
            "blockers": resume.get("blockers"),
        }
        step = recovered.call(
            "goal_next", {"session_id": session_id, "goal_id": evidence["goal_id"]}
        )
        evidence["after_resume_next"] = {
            "outcome": step.get("outcome"),
            "status": step.get("status"),
            "next_action": step.get("next_action"),
            "no_action_reason": step.get("no_action_reason"),
            "blockers": step.get("blockers"),
            "model_work_role": (step.get("model_work") or {}).get("role"),
        }
        result = recovered.call(
            "goal_result", {"session_id": session_id, "goal_id": evidence["goal_id"]}
        )
        evidence["goal_result"] = result
        final_status = recovered.call(
            "goal_status", {"session_id": session_id, "goal_id": evidence["goal_id"]}
        )
        evidence["final_status"] = final_status.get("status")
        evidence["file_after_recovery"] = (
            target.read_text(encoding="utf-8") if target.is_file() else None
        )
        evidence["file_matches_postimage"] = (
            hashlib.sha256(target.read_bytes()).hexdigest() == NEW_SHA256
            if target.is_file()
            else False
        )
        evidence["file_matches_preimage"] = (
            hashlib.sha256(target.read_bytes()).hexdigest() == preimage
            if target.is_file() and preimage
            else False
        )
        evidence["workspace_digest_after"] = base.workspace_digest(repo)
        evidence["durable_after_recovery"] = durable_quarantine(state)
        if evidence.get("critical_section", {}).get("publication_lock"):
            evidence["lock_after_recovery"] = lock_contents(
                Path(evidence["critical_section"]["publication_lock"])
            )
    finally:
        try:
            if recovered.child.poll() is None:
                recovered.child.terminate()
                recovered.child.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            pass
        if mode == "d2":
            # Only after the hostile state has been observed and recorded.
            if survivor is not None:
                (hold_dir / "survivor.release").write_text("stop\n", encoding="utf-8")
                survivor.wait(timeout=30)
                evidence["survivor"]["exited"] = not process_alive(survivor.pid)
            final = CrashServer(binary, state, repo, {})
            try:
                evidence["after_survivor_exit_resume"] = final.call(
                    "goal_resume",
                    {"session_id": session_id, "goal_id": evidence["goal_id"]},
                )
                step = final.call(
                    "goal_next",
                    {"session_id": session_id, "goal_id": evidence["goal_id"]},
                )
                evidence["after_survivor_exit_next"] = {
                    "outcome": step.get("outcome"),
                    "next_action": step.get("next_action"),
                    "no_action_reason": step.get("no_action_reason"),
                }
                evidence["durable_after_survivor_exit"] = durable_quarantine(state)
                if evidence.get("critical_section", {}).get("publication_lock"):
                    evidence["lock_after_survivor_exit"] = lock_contents(
                        Path(evidence["critical_section"]["publication_lock"])
                    )
            finally:
                if final.child.poll() is None:
                    final.child.terminate()
                    final.child.wait(timeout=10)
        evidence["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        fixture.evidence_path.write_text(
            json.dumps(evidence, indent=2), encoding="utf-8"
        )

    print(json.dumps({k: evidence.get(k) for k in (
        "mode",
        "critical_section",
        "after_restart",
        "after_resume_next",
        "final_status",
        "file_matches_postimage",
    )}, indent=2, default=str))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/local-mcp.exe")
    parser.add_argument("--mode", choices=("d1", "d2"), default="d1")
    parser.add_argument("--work-dir", default=None)
    parser.add_argument("--run-id", default=None)
    parser.add_argument("--hold-timeout", type=float, default=60.0)
    args = parser.parse_args()
    if args.run_id is None:
        args.run_id = f"crash-{args.mode}"
    return drive(args)


if __name__ == "__main__":
    sys.exit(main())
