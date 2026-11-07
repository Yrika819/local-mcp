"""E4-C dogfooding experiments 2 (disconnect/replay) and 3 (fail-closed inputs).

Run from the repository root:

    python scripts/e4c-dogfood/driver_exp23.py

`LOCAL_MCP_BINARY` / `LOCAL_MCP_E4C_WORK` override the built binary and the
scratch directory, exactly like `driver_exp1.py`.
"""

import json
import sys
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import driver_exp1 as d


def start_goal(client, session_id, objective):
    started = client.call(
        "goal_start",
        {
            "session_id": session_id,
            "objective": objective,
            "completion_criteria": [
                "inventory.txt was inspected read-only and its marker line is reported"
            ],
            "model_backend": "EXTERNAL",
        },
    )
    assert not started["is_error"], started
    return started["body"]["goal_id"]


def experiment_2(root: Path):
    repo = d.prepare_repo(root)
    session_id = f"e4c-reconnect-{uuid.uuid4().hex[:8]}"
    d.create_session(repo, session_id)
    out = {"session_id": session_id}

    client = d.McpClient(repo)
    goal_id = start_goal(client, session_id, "Reconnect and replay safety")
    planner = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    planner_work = planner["body"]["model_work"]
    proposal = d.planner_proposal(planner_work, "inventory-marker")
    planned = client.call(
        "goal_submit",
        d.submit_args(session_id, goal_id, planner_work, str(uuid.uuid4()), proposal),
    )
    assert planned["body"]["outcome"] == "COMMITTED", planned
    issued = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    work = issued["body"]["model_work"]
    assert work["role"] == "READONLY", issued
    out["issued_before_disconnect"] = {
        "work_id": work["work_id"],
        "request_digest": work["request_digest"],
        "state": work["state"],
        "attempt_id": work["attempt_id"],
    }
    # Simulated client disconnect: the MCP process ends, the Goal state is durable.
    client.close()

    client = d.McpClient(repo)
    reissued = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    out["after_reconnect"] = {
        "outcome": reissued["body"]["outcome"],
        "work_id": reissued["body"]["model_work"]["work_id"],
        "request_digest": reissued["body"]["model_work"]["request_digest"],
        "state": reissued["body"]["model_work"]["state"],
        "host_steps_executed": reissued["body"]["host_steps_executed"],
    }
    assert reissued["body"]["model_work"]["work_id"] == work["work_id"]
    assert reissued["body"]["model_work"]["request_digest"] == work["request_digest"]
    assert reissued["body"]["host_steps_executed"] == 0

    read = client.call("read_file", {"session_id": session_id, "path": "inventory.txt"})
    marker = [line for line in read["body"]["raw"].splitlines() if line.startswith("MARKER=")][0]
    answer = d.readonly_answer(reissued["body"]["model_work"], marker, "inventory-marker")
    submission_id = str(uuid.uuid4())
    first = client.call(
        "goal_submit",
        d.submit_args(session_id, goal_id, work, submission_id, answer),
    )
    assert first["body"]["outcome"] == "COMMITTED", first
    revision_after_first = client.call(
        "goal_status", {"session_id": session_id, "goal_id": goal_id}
    )["body"]["revision"]

    replay = client.call("goal_submit", d.submit_args(session_id, goal_id, work, submission_id, answer))
    assert replay["body"]["outcome"] == "REPLAY_OF_ACCEPTED_SUBMISSION", replay
    revision_after_replay = client.call(
        "goal_status", {"session_id": session_id, "goal_id": goal_id}
    )["body"]["revision"]
    out["replay"] = {
        "outcome": replay["body"]["outcome"],
        "revision_after_first": revision_after_first,
        "revision_after_replay": revision_after_replay,
        "receipt_committed_revision": replay["body"]["receipt"]["committed_revision"],
    }
    assert revision_after_first == revision_after_replay

    conflict = client.call(
        "goal_submit",
        d.submit_args(
            session_id, goal_id, work, submission_id, {**answer, "summary": "different payload"}
        ),
    )
    out["conflicting_duplicate"] = conflict["body"]["outcome"]
    assert conflict["body"]["outcome"] == "CONFLICTING_DUPLICATE", conflict

    finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    out["final"] = {
        "outcome": finished["body"]["outcome"],
        "status": finished["body"]["status"],
    }
    assert finished["body"]["status"] == "COMPLETED", finished
    client.close()
    out["workspace"] = d.git_state(repo)
    return out


def experiment_3(root: Path):
    repo = d.prepare_repo(root)
    session_id = f"e4c-adverse-{uuid.uuid4().hex[:8]}"
    d.create_session(repo, session_id)
    out = {"session_id": session_id}
    client = d.McpClient(repo)
    goal_id = start_goal(client, session_id, "Adversarial inputs must fail closed")

    # (a) unknown criterion id
    work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
    invented = d.planner_proposal(work, "inventory-marker")
    invented["criterion_bindings"][0]["criterion_id"] = str(uuid.uuid4())
    view = client.call("goal_submit", d.submit_args(session_id, goal_id, work, str(uuid.uuid4()), invented))
    out["unknown_criterion"] = {"outcome": view["body"]["outcome"], "code": view["body"]["code"]}
    assert view["body"]["outcome"] == "REJECTED_SCHEMA", view

    # (b) invalid work id
    work = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"]["model_work"]
    args = d.submit_args(
        session_id, goal_id, work, str(uuid.uuid4()), d.planner_proposal(work, "inventory-marker")
    )
    args["work_id"] = str(uuid.uuid4())
    view = client.call("goal_submit", args)
    out["invalid_work_id"] = {"outcome": view["body"]["outcome"], "code": view["body"]["code"]}
    assert view["body"]["outcome"] == "REJECTED_AUTHORITY", view

    # (c) stale revision
    args = d.submit_args(
        session_id, goal_id, work, str(uuid.uuid4()), d.planner_proposal(work, "inventory-marker")
    )
    args["expected_goal_revision"] = 1
    view = client.call("goal_submit", args)
    out["stale_revision"] = {"outcome": view["body"]["outcome"], "code": view["body"]["code"]}
    assert view["body"]["outcome"] == "REJECTED_STALE_REVISION", view

    # (d) the valid plan still commits on the same WorkItem
    view = client.call(
        "goal_submit",
        d.submit_args(
            session_id,
            goal_id,
            work,
            str(uuid.uuid4()),
            d.planner_proposal(work, "inventory-marker"),
        ),
    )
    out["valid_after_refusals"] = view["body"]["outcome"]
    assert view["body"]["outcome"] == "COMMITTED", view

    # (e) readonly answer with the required evidence missing
    readonly = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})["body"][
        "model_work"
    ]
    submission = str(uuid.uuid4())
    missing = d.readonly_answer(readonly, "MARKER=E4C-DOGFOOD-OK", "unrelated-requirement")
    view = client.call("goal_submit", d.submit_args(session_id, goal_id, readonly, submission, missing))
    out["readonly_missing_evidence_submitted"] = view["body"]["outcome"]
    assert view["body"]["outcome"] == "COMMITTED", view

    # (f) same submission_id, different payload
    conflict = client.call(
        "goal_submit",
        d.submit_args(
            session_id, goal_id, readonly, submission, {**missing, "summary": "tampered"}
        ),
    )
    out["same_submission_different_payload"] = conflict["body"]["outcome"]
    assert conflict["body"]["outcome"] == "CONFLICTING_DUPLICATE", conflict

    finished = client.call("goal_next", {"session_id": session_id, "goal_id": goal_id})
    out["after_missing_evidence"] = {
        "outcome": finished["body"]["outcome"],
        "status": finished["body"]["status"],
        "result_kind": finished["body"]["result_kind"],
    }
    assert finished["body"]["status"] != "COMPLETED", finished
    result = client.call("goal_result", {"session_id": session_id, "goal_id": goal_id})
    out["result_kind"] = result["body"]["result_kind"]
    assert result["body"]["result_kind"] != "COMPLETED"

    # (g) cancel then submit must not revive
    client.call("goal_cancel", {"session_id": session_id, "goal_id": goal_id})
    readonly2 = {
        "work_id": readonly["work_id"],
        "goal_revision": readonly["goal_revision"],
        "request_digest": readonly["request_digest"],
    }
    view = client.call(
        "goal_submit",
        d.submit_args(
            session_id,
            goal_id,
            readonly2,
            str(uuid.uuid4()),
            d.readonly_answer(readonly, "MARKER=E4C-DOGFOOD-OK", "inventory-marker"),
        ),
    )
    out["cancel_then_submit"] = {"outcome": view["body"]["outcome"], "code": view["body"]["code"]}
    assert view["body"]["outcome"] == "CANCELLED_STALE", view
    status = client.call("goal_status", {"session_id": session_id, "goal_id": goal_id})["body"]
    out["status_after_cancel"] = status["status"]
    assert status["status"] in ("CANCELLED", "CANCELLING"), status

    client.close()
    out["workspace"] = d.git_state(repo)
    return out


if __name__ == "__main__":
    base = d.WORK
    report = {
        "experiment_2_reconnect_replay": experiment_2(base / "exp2"),
        "experiment_3_adverse_inputs": experiment_3(base / "exp3"),
    }
    print(json.dumps(report, indent=2, ensure_ascii=False))
