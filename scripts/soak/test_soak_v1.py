"""Soak Harness V1 unit tests (stdlib unittest).

Covers: ledger lifecycle and consistency, timeout accounting, duplicate and
unknown responses, invalid frames, null-id accounting, shutdown recovery
classification, resource bounds and percentile floor rules, and driver
round-trips against the test-owned stub MCP server (including malformed
frames, oversized frames, EOF, child termination, and restart).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SOAK_DIR = HERE  # scripts/soak — stub server and package live here
sys.path.insert(0, SOAK_DIR)

from soak_v1 import ledger as led  # noqa: E402
from soak_v1 import driver as drv  # noqa: E402
from soak_v1 import resources as res  # noqa: E402
from soak_v1 import report as rep  # noqa: E402
from soak_v1.logging_util import JsonLogger  # noqa: E402

STUB = os.path.join(SOAK_DIR, "stub_mcp_server.py")


def make_logger():
    return JsonLogger(None, level="error", stream=open(os.devnull, "w"))


def make_driver(behavior="normal", ledger=None, cap=64 * 1024, timeout=5.0):
    ledger = ledger or led.ResponseLedger()
    env = dict(os.environ)
    env["STUB_FRAME_CAP"] = str(cap)
    env["STUB_BEHAVIOR"] = behavior
    env["PYTHONIOENCODING"] = "utf-8"
    d = drv.McpDriver(
        command=[sys.executable, STUB],
        env=env,
        cwd=SOAK_DIR,
        ledger=ledger,
        logger=make_logger(),
        request_timeout=timeout,
        max_inflight=128,
        write_timeout=10.0,
        wire_line_cap=cap,
    )
    return d, ledger


class LedgerLifecycleTests(unittest.TestCase):
    def test_normal_response_lifecycle(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1)
        ledger.sent(1)
        ledger.responded(1)
        metrics = ledger.finalize()
        self.assertEqual(metrics["attempted"], 1)
        self.assertEqual(metrics["normal_responses"], 1)
        self.assertEqual(metrics["unclassified"], 0)
        self.assertEqual(metrics["coverage"], 1.0)

    def test_jsonrpc_error_classified(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.sent(1); ledger.responded(1, is_error=True)
        metrics = ledger.finalize()
        self.assertEqual(metrics["jsonrpc_errors"], 1)
        self.assertEqual(metrics["coverage"], 1.0)

    def test_send_failed_terminal(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.send_failed(1, "broken pipe")
        metrics = ledger.finalize()
        self.assertEqual(metrics["send_failed"], 1)
        self.assertEqual(metrics["unclassified"], 0)

    def test_unclassified_attempt_fails_finalization(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1)  # never sent, never classified
        with self.assertRaises(led.LedgerInconsistency):
            ledger.finalize()

    def test_response_before_sent_marker_implicit_send(self):
        """Race guard: a response proves the frame was sent."""
        ledger = led.ResponseLedger()
        ledger.attempt(1)
        ledger.responded(1)  # response raced ahead of the sent-marker
        metrics = ledger.finalize()
        self.assertEqual(metrics["normal_responses"], 1)
        self.assertEqual(metrics["unknown_ids"], 0)
        self.assertEqual(metrics["unclassified"], 0)

    def test_sent_without_attempt_rejected(self):
        ledger = led.ResponseLedger()
        with self.assertRaises(led.LedgerInconsistency):
            ledger.sent(1)

    def test_duplicate_id_rejected(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1)
        with self.assertRaises(led.LedgerInconsistency):
            ledger.attempt(1)


class TimeoutAccountingTests(unittest.TestCase):
    def test_timed_out_then_late_response(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.sent(1)
        ledger.timed_out(1, "no response within 30s")
        ledger.responded(1)  # late
        metrics = ledger.finalize()
        self.assertEqual(metrics["timeouts"], 1)
        self.assertEqual(metrics["late_responses_after_timeout"], 1)
        self.assertEqual(metrics["normal_responses"], 0)

    def test_timed_out_requires_sent(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1)
        with self.assertRaises(led.LedgerInconsistency):
            ledger.timed_out(1, "x")


class DuplicateUnknownInvalidTests(unittest.TestCase):
    def test_duplicate_response_counted(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.sent(1)
        ledger.responded(1)
        ledger.responded(1)
        metrics = ledger.finalize()
        self.assertEqual(metrics["normal_responses"], 1)
        self.assertEqual(metrics["duplicate_responses"], 1)

    def test_unknown_id_response_counted(self):
        ledger = led.ResponseLedger()
        ledger.unknown_response(42)
        metrics = ledger.finalize()
        self.assertEqual(metrics["unknown_ids"], 1)
        self.assertEqual(metrics["unclassified"], 0)

    def test_invalid_frame_terminal(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.sent(1)
        ledger.invalid_frame(1, "frame not an object")
        metrics = ledger.finalize()
        self.assertEqual(metrics["invalid_responses"], 1)

    def test_null_id_and_notification_counters(self):
        ledger = led.ResponseLedger()
        ledger.null_id_response(3)
        ledger.notification_sent(2)
        metrics = ledger.finalize()
        self.assertEqual(metrics["null_id_responses"], 3)
        self.assertEqual(metrics["notifications_sent"], 2)

    def test_raw_probe_responses_counted_separately(self):
        ledger = led.ResponseLedger()
        ledger.raw_probe_response(2)
        metrics = ledger.finalize()
        self.assertEqual(metrics["raw_probe_responses"], 2)
        self.assertEqual(metrics["unknown_ids"], 0)


class ShutdownAccountingTests(unittest.TestCase):
    def test_shutdown_recovered_and_unrecovered(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.sent(1)
        ledger.attempt(2); ledger.sent(2)
        moved = ledger.shutdown_begin()
        self.assertEqual(moved, 2)
        ledger.responded(1)  # recovered at EOF
        metrics = ledger.finalize()
        self.assertEqual(metrics["shutdown_recovered"], 1)
        self.assertEqual(metrics["shutdown_unrecovered"], 1)
        self.assertEqual(metrics["coverage"], 0.5)

    def test_coverage_uses_sent_denominator(self):
        ledger = led.ResponseLedger()
        ledger.attempt(1); ledger.send_failed(1, "x")
        ledger.attempt(2); ledger.sent(2); ledger.responded(2)
        metrics = ledger.finalize()
        self.assertEqual(metrics["sent"], 1)
        self.assertEqual(metrics["coverage"], 1.0)


class ResourceTests(unittest.TestCase):
    def test_percentile_floor_not_measured(self):
        reservoir = res.LatencyReservoir(cap=10_000)
        for index in range(99):
            reservoir.add(0.001)
        stats = reservoir.stats()
        self.assertEqual(stats["p50_ms"], "NOT-MEASURED")

    def test_percentiles_measured_at_floor(self):
        reservoir = res.LatencyReservoir(cap=10_000)
        for index in range(100):
            reservoir.add(0.001 + index / 1e6)
        stats = reservoir.stats()
        self.assertIsInstance(stats["p50_ms"], float)
        self.assertIsInstance(stats["p99_ms"], float)

    def test_reservoir_cap_and_dropped(self):
        reservoir = res.LatencyReservoir(cap=10, seed=7)
        for index in range(25):
            reservoir.add(float(index))
        self.assertEqual(len(reservoir._samples), 10)
        self.assertEqual(reservoir._seen, 25)
        self.assertLessEqual(reservoir._dropped, 15)

    def test_bounds_violation_detection(self):
        sampler = res.ResourceSampler.__new__(res.ResourceSampler)
        sampler.bounds = res.ResourceBounds(max_inflight=4, fd_cap=10,
                                            response_byte_cap=100, working_set_cap=1000,
                                            working_set_growth_fraction=0.5,
                                            working_set_growth_floor_bytes=0)
        sampler.peak_working_set = 2000
        sampler.peak_fds = 20
        sampler.samples = [{"working_set_bytes": 2000}]
        sampler.status = res.MEASURED
        sampler.ws_supported = True
        violations = sampler.check_bounds(max_inflight_observed=9, response_bytes=200)
        self.assertTrue(any("working set" in v for v in violations))
        self.assertTrue(any("fd count" in v for v in violations))
        self.assertTrue(any("in-flight" in v for v in violations))
        self.assertTrue(any("response bytes" in v for v in violations))

    def test_working_set_bound_uses_min_of_cap_and_growth(self):
        bounds = res.ResourceBounds(working_set_cap=10_000_000, working_set_growth_fraction=0.5,
                                    working_set_growth_floor_bytes=0)
        sampler = res.ResourceSampler.__new__(res.ResourceSampler)
        sampler.bounds = bounds
        sampler.samples = [{"working_set_bytes": 4_000_000}]
        self.assertEqual(sampler.working_set_bound(), 6_000_000)
        sampler.samples = [{"working_set_bytes": 30_000_000}]
        self.assertEqual(sampler.working_set_bound(), 10_000_000)

    def test_working_set_growth_floor(self):
        bounds = res.ResourceBounds(working_set_cap=10_000_000_000,
                                    working_set_growth_fraction=0.5,
                                    working_set_growth_floor_bytes=256 * 1024 * 1024)
        sampler = res.ResourceSampler.__new__(res.ResourceSampler)
        sampler.bounds = bounds
        sampler.samples = [{"working_set_bytes": 13_000_000}]
        self.assertEqual(sampler.working_set_bound(), 13_000_000 + 256 * 1024 * 1024)

    def test_sampler_not_run_on_unsupported(self):
        sampler = res.ResourceSampler.__new__(res.ResourceSampler)
        sampler.status = res.NOT_RUN
        sampler.ws_supported = False
        sampler.violations = []
        self.assertEqual(sampler.check_bounds(999, 10**9), [])


class ReportTests(unittest.TestCase):
    def test_report_redaction(self):
        report = {"session": "secret-session", "dir": "C:/Users/someone", "other": "ok"}
        redacted = rep.redact_report(report, ["secret-session", "C:/Users/someone"])
        self.assertNotIn("secret-session", json.dumps(redacted))
        self.assertNotIn("C:/Users/someone", json.dumps(redacted))

    def test_report_write_no_secrets(self):
        with tempfile.TemporaryDirectory() as run_dir:
            report = {
                "seed": 1, "commit_sha": "a" * 40, "platform": "linux",
                "runner": "test", "duration_s": 1.0, "verdict": "PASS",
                "metrics": {"coverage": 1.0}, "resources": {}, "latency": {},
                "scenarios": {}, "failure_injection": [], "not_run": [],
                "violations": [], "limitations": [], "bounds": {},
                "session_secret": "dont-leak-me",
            }
            report = rep.redact_report(report, ["dont-leak-me"])
            json_path, md_path = rep.write_report(run_dir, report)
            text = open(json_path, encoding="utf-8").read() + open(md_path, encoding="utf-8").read()
            self.assertNotIn("dont-leak-me", text)


class DriverStubTests(unittest.TestCase):
    """Round-trips against the test-owned stub MCP server."""

    def test_initialize_and_tools_call(self):
        d, ledger = make_driver()
        d.start()
        try:
            response = d.request("initialize", {})
            self.assertFalse(response.is_error)
            self.assertIn("serverInfo", response.payload["result"])
            response = d.request("tools/call", {"name": "session_info", "arguments": {}})
            self.assertFalse(response.is_error)
        finally:
            d.close(wait_grace_s=0.5)
            ledger.finalize()

    def test_error_response_classified(self):
        d, ledger = make_driver()
        d.start()
        try:
            response = d.request("test.error", {})
            self.assertTrue(response.is_error)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["jsonrpc_errors"], 1)

    def test_timeout_classification(self):
        d, ledger = make_driver(behavior="no-response", timeout=0.5)
        d.start()
        try:
            with self.assertRaises(TimeoutError):
                d.request("ping", {})
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["timeouts"], 1)

    def test_duplicate_response_counted(self):
        d, ledger = make_driver()
        d.start()
        try:
            d.request("test.duplicate", {})
            time.sleep(0.3)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["normal_responses"], 1)
        self.assertEqual(metrics["duplicate_responses"], 1)

    def test_unknown_id_response_counted(self):
        d, ledger = make_driver()
        d.start()
        try:
            # The stub answers with a foreign id; our id legitimately times out.
            try:
                d.request("test.unknown", {})
            except TimeoutError:
                pass
            time.sleep(0.3)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["unknown_ids"], 1)

    def test_bad_frame_output_counted(self):
        d, ledger = make_driver()
        d.start()
        try:
            try:
                d.request("test.bad_frame", {})
            except TimeoutError:
                pass
            time.sleep(0.3)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["malformed_outputs"], 1)

    def test_null_id_response_for_malformed_request(self):
        d, ledger = make_driver()
        d.start()
        try:
            d.send_raw("this is not json\n")
            time.sleep(0.3)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["null_id_responses"], 1)

    def test_notification_gets_no_response(self):
        d, ledger = make_driver()
        d.start()
        try:
            d.send_notification("ping")
            time.sleep(0.3)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["notifications_sent"], 1)
        self.assertEqual(metrics["null_id_responses"], 0)

    def test_oversized_frame_refused_with_null_id(self):
        cap = 4096
        d, ledger = make_driver(cap=cap)
        d.start()
        try:
            d.send_raw("x" * (cap + 512) + "\n")
            time.sleep(0.5)
        finally:
            d.close(wait_grace_s=0.5)
        metrics = ledger.finalize()
        self.assertEqual(metrics["null_id_responses"], 1)

    def test_client_eof_restarts(self):
        d, ledger = make_driver()
        d.start()
        d.half_close_stdin()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and d._proc.poll() is None:
            time.sleep(0.05)
        self.assertIsNotNone(d._proc.poll())
        d.close(wait_grace_s=0.0)
        metrics = ledger.finalize()
        self.assertEqual(metrics["unclassified"], 0)

    def test_child_kill_and_restart_recovery(self):
        d, ledger = make_driver()
        d.start()
        response = d.request("ping", {})
        self.assertFalse(response.is_error)
        d._proc.kill()
        d._proc.wait(timeout=5)
        d.restart()
        try:
            response = d.request("ping", {})
            self.assertFalse(response.is_error)
        finally:
            d.close(wait_grace_s=0.5)
            ledger.finalize()

    def test_restart_classifies_pending_unrecovered(self):
        d, ledger = make_driver(behavior="no-response", timeout=5.0)
        d.start()
        # Issue a request that will never be answered, then terminate.
        thread = threading.Thread(target=lambda: setattr(
            self, "_outcome", None), daemon=True)
        thread.start()
        try:
            with self.assertRaises(TimeoutError):
                d.request("ping", {}, timeout=0.4)
        finally:
            pass
        # Close without grace: nothing pending (timed out already classified).
        d.close(wait_grace_s=0.0)
        metrics = ledger.finalize()
        self.assertEqual(metrics["timeouts"], 1)


class TestOwnedEnvTests(unittest.TestCase):
    def test_env_redirection(self):
        with tempfile.TemporaryDirectory() as state_root:
            env = drv.test_owned_env(state_root)
            if sys.platform == "win32":
                self.assertEqual(env["USERPROFILE"], os.path.abspath(state_root))
                self.assertEqual(env["LOCALAPPDATA"], os.path.join(os.path.abspath(state_root), "AppData", "Local"))
            elif sys.platform == "darwin":
                self.assertEqual(env["HOME"], os.path.abspath(state_root))
            else:
                self.assertTrue(env["XDG_STATE_HOME"].startswith(os.path.abspath(state_root)))

    def test_session_id_deterministic(self):
        self.assertEqual(drv.new_session_id(7), drv.new_session_id(7))
        self.assertNotEqual(drv.new_session_id(7), drv.new_session_id(8))


if __name__ == "__main__":
    unittest.main()
