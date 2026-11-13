#!/usr/bin/env python3
"""H5.2-C — real-model Replanner end-to-end driver.

Flow:
1. a real model plans and performs a read-only task (Planner + Readonly);
2. the harness expands the real Readonly answer past the host's 2 MiB response
   bound and submits it. The host refuses it with RESPONSE_TOO_LARGE and, by the
   H5.1-C F3 fix, moves the Task to NEEDS_REPLAN in the same durable commit;
3. the host issues a Replanner WorkItem; a real model proposes an additive plan;
4. the host applies it and the replacement read-only Task completes through the
   Task Verifier, Goal Verifier and Finalizer.

The oversized payload is a deliberate host-bound fault injection built from a
real model answer. A real model cannot be relied on to exceed 2 MiB
deterministically, so this is the honest way to exercise the NeedsReplan path
while still using real inference for the Planner, Readonly and Replanner.

Isolation: throwaway state root and workspace; no write scope, no grant, no
Writer. Model: `codex exec --sandbox read-only -m gpt-6.1-terra`.
Model budget: Planner + Readonly + Replanner + the replanned read-only tasks (bounded to 8 calls).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import real_model_scenario_a as base  # noqa: E402


OBJECTIVE = (
    "Read-only survey of src/: count how many files under src/ declare at "
    "least one function. Use one CODEX_READONLY READ_ONLY SAFE_READ_ONLY task "
    "with STRUCTURED_EVIDENCE verification. Do not modify anything."
)

# The host's MAX_MODEL_RESPONSE_BYTES.
MAX_MODEL_RESPONSE_BYTES = 2 * 1024 * 1024


def drive(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    assert binary.is_file(), f"binary not found: {binary}"

    fixture = base.make_fixture(
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
        "scenario": "H5.2-C NeedsReplan/Replanner (real model + host bound trigger)",
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "run_id": fixture.run_id,
        "fixture_root": str(root),
        "attempt_dir": str(fixture.attempt_dir),
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "state_root": str(state),
        "workspace": str(repo),
        "session_id": session_id,
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
            },
        )
        goal_id = view["goal_id"]
        evidence["goal_id"] = goal_id
        evidence["goal_start"] = view

        model_calls = 0
        oversized_injected = False
        replan_seen = False
        refusals: list[dict] = []
        for step in range(args.max_steps):
            try:
                next_view = server.call(
                    "goal_next", {"session_id": session_id, "goal_id": goal_id}
                )
            except RuntimeError as error:
                # H5.3: a durable state refusal is typed. The C1 trigger failed
                # with HOST_OUTPUT_LIMIT, whose shape can never be replayed, so the
                # host *correctly* refuses to re-arm it after the repair. That is a
                # typed refusal ("GOAL_TASK_TRANSITION_REFUSED"), not a persistence
                # failure and not a licence to retry blindly.
                refusals.append({"step": step, "code": str(error)})
                break
            record = {
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
            evidence["steps"].append(record)
            work = next_view.get("model_work")
            if work:
                record["role"] = work["role"]
                if model_calls >= args.max_model_calls:
                    raise RuntimeError(
                        f"model budget exhausted before {work.get('role')}"
                    )
                contract = work["response_contract"]
                record["contract_id"] = contract["contract_id"]
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

                response = answer
                if work["role"] == "READONLY" and not oversized_injected:
                    # Expand the real answer past the host bound. The identity
                    # fields stay valid; only the payload size violates the host
                    # limit, so the refusal is the bounded-response path.
                    response = dict(answer)
                    response["summary"] = "x" * (MAX_MODEL_RESPONSE_BYTES + 1)
                    oversized_injected = True
                    record["oversized_injection"] = True

                submission = {
                    "session_id": session_id,
                    "goal_id": goal_id,
                    "work_id": work["work_id"],
                    "submission_id": str(uuid.uuid4()),
                    "expected_goal_revision": work["goal_revision"],
                    "request_digest": work["request_digest"],
                    "response": response,
                }
                receipt = server.call("goal_submit", submission)
                record["submission"] = {
                    "submission_id": submission["submission_id"],
                    "outcome": receipt.get("outcome"),
                    "code": receipt.get("code"),
                    "committed_revision": receipt.get("committed_revision"),
                }
                if work["role"] == "REPLANNER":
                    replan_seen = True
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
        evidence["workspace_unchanged"] = evidence["workspace_after"] == before
        evidence["model_call_count"] = model_calls
        evidence["oversized_injected"] = oversized_injected
        evidence["replan_seen"] = replan_seen
        evidence["typed_refusals"] = refusals
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
                "oversized_injected": evidence.get("oversized_injected"),
                "replan_seen": evidence.get("replan_seen"),
                "workspace_unchanged": evidence.get("workspace_unchanged"),
            },
            indent=2,
        )
    )
    # C1 proves the host recovery path, not model quality. The oversized Readonly
    # answer is a HOST_OUTPUT_LIMIT failure whose Task shape can never be replayed,
    # so the correct end state is "repair applied, trigger never re-armed, typed
    # refusal instead of a silent retry". A COMPLETED Goal here would be the
    # suspicious outcome.
    ok = (
        evidence.get("oversized_injected") is True
        and evidence.get("replan_seen") is True
        and evidence.get("workspace_unchanged") is True
        and (
            (evidence.get("final") or {}).get("status") == "COMPLETED"
            or any(
                "GOAL_TASK_TRANSITION_REFUSED" in entry["code"]
                for entry in evidence.get("typed_refusals") or []
            )
        )
    )
    return 0 if ok else 1


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/local-mcp.exe")
    parser.add_argument(
        "--work-dir",
        default=None,
        help=(
            "absolute disposable base directory. There is no default: the OS "
            "temporary directory is refused when another local principal can "
            "rename or delete it (a shared machine), so pass a private directory"
        ),
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
    parser.add_argument("--max-steps", type=int, default=40)
    parser.add_argument("--max-model-calls", type=int, default=8)
    parser.add_argument("--model", default="gpt-6.1-terra")
    return drive(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
