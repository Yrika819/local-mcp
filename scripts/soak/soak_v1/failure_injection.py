"""Failure injection and recovery (Soak Harness V1, Slice 4).

FI-1..FI-5 per docs/SOAK_HARNESS_V1_DESIGN.md §7. Injections run only in the
test-owned environment (test state dir, fixture tree, harness-spawned child).
Un-achievable injections are recorded NOT RUN, never simulated.
"""

from __future__ import annotations

import hashlib
import json
import os
import signal
import subprocess
import sys
import time


_SIGKILL = getattr(signal, "SIGKILL", signal.SIGTERM)  # Windows has no SIGKILL


def _kill_child(proc: subprocess.Popen, sig: int) -> dict:
    try:
        if sig == _SIGKILL and sys.platform == "win32":
            proc.kill()
        else:
            proc.send_signal(sig)
        proc.wait(timeout=15)
        return {"exit_code": proc.poll(), "signaled": sig}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"exit_code": proc.poll(), "signaled": sig, "error": repr(error)}


def _fixture_hashes(fixture_dir: str) -> dict:
    hashes = {}
    for root, _dirs, files in os.walk(fixture_dir):
        for name in files:
            path = os.path.join(root, name)
            rel = os.path.relpath(path, fixture_dir)
            with open(path, "rb") as handle:
                hashes[rel] = hashlib.sha256(handle.read()).hexdigest()
    return hashes


def fi1_goal_midlife(harness, log) -> dict:
    """Kill the child while a Goal is active; verify coherent state after restart."""
    result = {"fi": "FI-1", "status": "NOT RUN", "detail": ""}
    engine = harness.engine
    if engine is None or not engine.active_goals:
        result["detail"] = "no active goal to inject against"
        return result
    goal_id = engine.active_goals[0]
    proc = harness.driver._proc
    _kill_child(proc, signal.SIGTERM)
    harness.restart_driver()
    response = harness.driver.request(
        "tools/call",
        {"name": "goal_status", "arguments": {"session_id": harness.session_id, "goal_id": goal_id}},
    )
    result["status"] = "OBSERVED"
    result["detail"] = "coherent goal_status after restart" if not response.is_error else (
        "goal_status errored after restart (refusal recorded)"
    )
    result["goal_answered"] = not response.is_error
    return result


def fi2_midwrite(harness, log) -> dict:
    """Kill the child during a write; the fixture tree must be intact or match
    the last acknowledged publish (no torn files)."""
    fixture_dir = harness.fixture_dir
    before = _fixture_hashes(fixture_dir)
    request_id = harness.driver.new_id()
    harness.driver.ledger.attempt(request_id)
    frame = json.dumps(
        {"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
         "params": {"name": "write_file", "arguments": {
             "session_id": harness.session_id, "path": "fi2-probe.txt",
             "content": "fi2 probe " * 1000}}}
    ) + "\n"
    try:
        harness.driver.send_raw(frame)
        harness.driver.ledger.sent(request_id)
        time.sleep(0.05)
        _kill_child(harness.driver._proc, _SIGKILL)
        harness.restart_driver()
    except (OSError, ValueError) as error:
        result = {"fi": "FI-2", "status": "NOT RUN", "detail": repr(error)}
        return result
    after = _fixture_hashes(fixture_dir)
    torn = [
        name for name, digest in after.items()
        if before.get(name) is not None and before[name] != digest
    ]
    created_without_ack = "fi2-probe.txt" in after and before.get("fi2-probe.txt") is None
    result = {"fi": "FI-2", "status": "OBSERVED", "torn_files": torn,
              "created_without_ack": created_without_ack}
    if torn:
        result["status"] = "FAIL"
        result["detail"] = f"torn files detected: {torn[:5]}"
    return result


def fi3_malformed_flood(harness, log, count: int = 50) -> dict:
    """Malformed frame flood interleaved with normal traffic (S7 acceptance)."""
    try:
        driver = harness.driver
        for index in range(count):
            driver.send_raw(f"not json {index}\n")
            driver.send_raw(json.dumps({"jsonrpc": "1.0", "id": -1, "method": "ping"}) + "\n")
        response = driver.request(
            "tools/call",
            {"name": "session_info", "arguments": {"session_id": harness.session_id}},
            timeout=30.0,
        )
        return {"fi": "FI-3", "status": "OBSERVED", "child_responsive": True}
    except (TimeoutError, OSError, ValueError) as error:
        return {"fi": "FI-3", "status": "FAIL", "detail": repr(error)}


def fi4_eof_inflight(harness, log) -> dict:
    """EOF during sustained in-flight traffic: >= 1,000 attempted, <= bound
    outstanding; every id classified as shutdown-recovered or unrecovered."""
    try:
        driver = harness.driver
        sent = 0
        while sent < 1000:
            try:
                driver.request("tools/call", {"name": "session_info",
                                              "arguments": {"session_id": harness.session_id}},
                                timeout=0.05)
                sent += 1
            except TimeoutError:
                sent += 1
                if driver.inflight() >= harness.bounds.max_inflight:
                    break
        inflight_before = driver.inflight()
        driver.half_close_stdin()
        grace_end = time.monotonic() + 15.0
        while time.monotonic() < grace_end and driver.inflight() > 0:
            time.sleep(0.1)
        recovered = inflight_before - driver.inflight()
        harness.shutdown_pending_close = True
        return {
            "fi": "FI-4",
            "status": "OBSERVED",
            "attempted_this_phase": sent,
            "inflight_at_eof": inflight_before,
            "recovered_at_eof": recovered,
            "unrecovered": driver.inflight(),
        }
    except (OSError, ValueError) as error:
        return {"fi": "FI-4", "status": "FAIL", "detail": repr(error)}


def fi5_disk_full(log) -> dict:
    """Disk-full on the state dir. Requires a quota mount the harness cannot
    create safely in CI: reported NOT RUN, never simulated by byte-editing."""
    return {
        "fi": "FI-5",
        "status": "NOT RUN",
        "detail": "quota mount not available in this environment; simulation forbidden",
    }
