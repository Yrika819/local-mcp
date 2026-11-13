#!/usr/bin/env python3
"""H5.3-B / C2 — real-task Replanner end to end (no host fault injection).

`real_model_scenario_c.py` (C1) proves the *host* recovery path by expanding a
real Readonly answer past the 2 MiB response bound. That is a deliberate
fault-injection test of the host, and it says nothing about whether a real model
can produce a Replanner answer the host accepts.

This driver is the other half (C2). Nothing is injected:

1. the workspace genuinely lacks the input the objective names
   (`data/ledger.csv`), and the driver proves that absence before the run;
2. a real model plans and then answers the Readonly Task. Because the input is
   genuinely absent, the honest answer is `status: needs_replan` — a
   model-authored, evidence-based replan request;
3. the host moves the Task to NEEDS_REPLAN and issues a real Replanner WorkItem;
4. a real model must produce a Replanner answer that the host validator accepts:
   a Writer repair Task that creates `data/ledger.csv`, a new hard dependency
   from the blocked Task to that repair Task, and a
   `resolve_needs_replan` entry. The driver never edits that answer;
5. the Writer publishes the file through the production publication path using
   the H5-C disposable TEST_FIXTURE_V1 grant;
6. the blocked Readonly Task re-runs against the repaired workspace and
   completes through the host Task Verifier;
7. the Goal reaches COMPLETED.

Every answer the host receives is exactly what `codex gpt-6.1-terra` produced.

Model budget: explicit, bounded (`--max-model-calls`, default 12).
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
import real_model_scenario_b as grants  # noqa: E402


WRITE_SCOPE = {
    "allowed_paths": ["data/ledger.csv"],
    "allowed_operations": ["WRITE_UTF8"],
    "max_operations": 2,
    "max_bytes": 4096,
    "expires_in_seconds": 900,
}

LEDGER_CONTENT = "widgets=5\ngadgets=3\n"
LEDGER_SHA256 = hashlib.sha256(LEDGER_CONTENT.encode("utf-8")).hexdigest()

OBJECTIVE = (
    "Read-only inventory audit across two input files.\n"
    "\n"
    "Plan exactly one CODEX_READONLY Task, mandatory, with scope allowed_paths "
    "['.'], forbidden_paths [], operation_kind READ_ONLY, replay_safety "
    "SAFE_READ_ONLY, and one STRUCTURED_EVIDENCE verification entry whose "
    "requirement_id is 'inventory_total'.\n"
    "\n"
    "That Task must report the total of the numeric values in data/inventory.txt "
    "plus the total of the numeric values in data/ledger.csv, as a single "
    "evidence item whose kind is 'inventory_total'.\n"
    "\n"
    "data/ledger.csv is NOT present in the workspace. An absent input named by "
    "this objective is a plan-level gap, not a task-level blocker: when an input "
    "is absent, answer the Readonly Task with status 'needs_replan' and a summary "
    "that names the absent input. Do not answer 'blocked'. Do not invent values "
    "and do not claim the audit is complete.\n"
    "\n"
    "The host will then issue a Replanner WorkItem for the resulting NEEDS_REPLAN "
    "Task. The Replanner must add one CODEX_WRITER Task that creates "
    "data/ledger.csv with exactly this UTF-8 content:\n"
    + LEDGER_CONTENT
    + "The Writer Task scope must be allowed_paths ['data/ledger.csv'], "
    "forbidden_paths [], operation_kind LOCAL_MUTATION, replay_safety "
    "VERIFY_BEFORE_RETRY, and its verification must be FILE_DIGEST for "
    "data/ledger.csv with expected_sha256 "
    + LEDGER_SHA256
    + ". The Replanner must point the blocked read-only Task at that Writer Task "
    "with an add_dependencies entry and resolve the blocked Task with "
    "resolve_needs_replan."
)


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

    # H5.3-B2: prove the workspace premise before the model is told anything.
    inventory = repo / "data" / "inventory.txt"
    ledger = repo / "data" / "ledger.csv"
    premise = {
        "inventory_present": inventory.is_file(),
        "inventory_sha256": hashlib.sha256(inventory.read_bytes()).hexdigest()
        if inventory.is_file()
        else None,
        "ledger_present_before": ledger.is_file(),
        "model_cwd": str(repo.resolve()),
        "readable_files": sorted(
            str(path.relative_to(repo)).replace("\\", "/")
            for path in repo.rglob("*")
            if path.is_file()
        ),
    }
    assert premise["inventory_present"], "the fixture must provide data/inventory.txt"
    assert not premise["ledger_present_before"], (
        "data/ledger.csv must genuinely be absent; the needs_replan must be true"
    )

    fixture_id = grants.write_fixture_marker(root, state, repo)
    evidence: dict = {
        "scenario": "H5.3-B C2 (real-model needs_replan + Writer repair, no injection)",
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "run_id": fixture.run_id,
        "fixture_root": str(root),
        "attempt_dir": str(fixture.attempt_dir),
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "state_root": str(state),
        "workspace": str(repo),
        "session_id": session_id,
        "fixture_id": fixture_id,
        "model": args.model,
        "reasoning": "high",
        "host_fault_injection": "none",
        "workspace_premise": premise,
        "write_scope": WRITE_SCOPE,
        "ledger_expected_sha256": LEDGER_SHA256,
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

        evidence["test_grant"] = grants.issue_fixture_grant(
            binary, state, repo, session_id, goal_id
        )

        model_calls = 0
        seen_roles: list[str] = []
        needs_replan_seen = False
        replan_applied = False

        for step in range(args.max_steps):
            next_view = server.call(
                "goal_next", {"session_id": session_id, "goal_id": goal_id}
            )
            record = {
                "step": step,
                "outcome": next_view.get("outcome"),
                "status": next_view.get("status"),
                "revision": next_view.get("revision"),
                "plan_revision": next_view.get("plan_revision"),
                "next_action": next_view.get("next_action"),
                "no_action_reason": next_view.get("no_action_reason"),
                "blockers": next_view.get("blockers"),
                "model_work_id": (next_view.get("model_work") or {}).get("work_id"),
            }
            evidence["steps"].append(record)
            work = next_view.get("model_work")
            if work:
                role = work["role"]
                seen_roles.append(role)
                record["role"] = role
                if model_calls >= args.max_model_calls:
                    raise RuntimeError(f"model budget exhausted before {role}")
                contract = work["response_contract"]
                record["contract_id"] = contract["contract_id"]
                record["conditions"] = [
                    entry["code"] for entry in contract.get("conditions") or []
                ]
                record["notes_forwarded"] = len(contract.get("notes") or [])
                digest_before = base.workspace_digest(repo)
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
                # H5.3-B2: a role that has no write authority must not change the
                # workspace. This is checked on every call, so a wider model
                # sandbox cannot silently become a write channel.
                digest_after = base.workspace_digest(repo)
                meta["workspace_unchanged_by_this_call"] = digest_before == digest_after
                if role != "WRITER" and digest_before != digest_after:
                    changed = sorted(
                        set(digest_before) ^ set(digest_after)
                        | {
                            key
                            for key in set(digest_before) & set(digest_after)
                            if digest_before[key] != digest_after[key]
                        }
                    )
                    raise RuntimeError(
                        f"the {role} model call changed the workspace: {changed}"
                    )
                evidence.setdefault("model_calls", []).append(meta)
                record["answer"] = answer

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
                record["submission"] = {
                    "submission_id": submission["submission_id"],
                    "outcome": receipt.get("outcome"),
                    "code": receipt.get("code"),
                    "committed_revision": receipt.get("committed_revision"),
                }
                if role == "READONLY" and receipt.get("code") == "READONLY_NEEDS_REPLAN":
                    needs_replan_seen = True
                    record["needs_replan_origin"] = "real model status=needs_replan"
                if role == "REPLANNER" and receipt.get("code") == "REPLAN_APPLIED":
                    replan_applied = True
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
        after = base.workspace_digest(repo)
        evidence["workspace_after"] = after
        evidence["changed_paths"] = grants.changed_paths(before, after)
        evidence["ledger_present_after"] = ledger.is_file()
        evidence["ledger_sha256_after"] = (
            hashlib.sha256(ledger.read_bytes()).hexdigest() if ledger.is_file() else None
        )
        evidence["model_call_count"] = model_calls
        evidence["roles_seen"] = seen_roles
        evidence["needs_replan_seen"] = needs_replan_seen
        evidence["replan_applied"] = replan_applied
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
                "roles_seen": evidence.get("roles_seen"),
                "needs_replan_seen": evidence.get("needs_replan_seen"),
                "replan_applied": evidence.get("replan_applied"),
                "changed_paths": evidence.get("changed_paths"),
                "ledger_sha256_after": evidence.get("ledger_sha256_after"),
            },
            indent=2,
        )
    )
    ok = (
        (evidence.get("final") or {}).get("status") == "COMPLETED"
        and evidence.get("needs_replan_seen") is True
        and evidence.get("replan_applied") is True
        and evidence.get("changed_paths") == ["data/ledger.csv"]
        and evidence.get("ledger_sha256_after") == LEDGER_SHA256
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
    parser.add_argument("--run-id", default=None)
    parser.add_argument("--max-steps", type=int, default=40)
    parser.add_argument("--max-model-calls", type=int, default=12)
    parser.add_argument("--model", default="gpt-6.1-terra")
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
    return drive(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
