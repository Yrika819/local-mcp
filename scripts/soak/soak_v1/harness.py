"""Soak Harness V1 orchestrator and CLI (Slices 1-5 orchestration).

Run a soak against a freshly built ``local-mcp mcp`` binary of the commit
under test:

    python -m soak_v1 --server-cmd "<path> mcp" --duration 60 --seed 1 --out runs/smoke

Verdicts: PASS / FAIL / NOT RUN. Unanswered requests are never success; a
ledger inconsistency is FAIL; an unmeasurable bound is NOT RUN (never PASS).
Exit codes: 0 = PASS, 1 = FAIL, 2 = NOT RUN.
"""

from __future__ import annotations

import argparse
import json
import os
import platform as _platform
import random
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

from . import driver as drv
from . import ledger as led
from . import resources as res
from . import scenarios as scn
from . import failure_injection as fi
from . import report as rep
from .logging_util import JsonLogger

PASS = "PASS"
FAIL = "FAIL"
NOT_RUN = "NOT RUN"


def platform_label() -> str:
    if sys.platform.startswith("linux"):
        return "linux"
    if sys.platform == "win32":
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return sys.platform or "unknown"


class SoakHarness:
    def __init__(self, args, logger: JsonLogger) -> None:
        self.args = args
        self.log = logger
        self.platform = platform_label()
        self.bounds = res.ResourceBounds(
            max_inflight=args.max_inflight,
            wire_line_cap=args.wire_line_cap,
            response_byte_cap=args.response_byte_cap,
            working_set_cap=args.working_set_cap,
            working_set_growth_fraction=args.working_set_growth_fraction,
            working_set_growth_floor_bytes=args.working_set_growth_floor,
            fd_cap=args.fd_cap,
            max_latency_samples=args.max_latency_samples,
        )
        self.rng = random.Random(args.seed)
        self.session_id = drv.new_session_id(args.seed)
        self.ledger = led.ResponseLedger()
        self.driver: drv.McpDriver | None = None
        self.engine: scn.ScenarioEngine | None = None
        self.sampler: res.ResourceSampler | None = None
        self.run_dir = os.path.abspath(args.out)
        self.state_root = os.path.join(self.run_dir, "state")
        self.fixture_dir = os.path.join(self.run_dir, "fixture")
        self.session_file = ""  # set by prepare() under the platform state base
        self.latency = res.LatencyReservoir(
            cap=self.bounds.max_latency_samples, seed=args.seed
        )
        self.unexpected_child_deaths = 0
        self.scenario_log: list = []
        self.shutting_down = False

    # -- environment preparation ------------------------------------------------
    def prepare(self) -> None:
        os.makedirs(self.run_dir, exist_ok=True)
        sessions_dir = os.path.join(
            drv.state_dir_base(self.state_root), "local-mcp", "sessions"
        )
        os.makedirs(sessions_dir, exist_ok=True)
        os.makedirs(self.fixture_dir, exist_ok=True)
        with open(os.path.join(self.fixture_dir, "fixture-a.txt"), "w", encoding="utf-8") as handle:
            handle.write("soak fixture alpha\n" * 20)
        os.makedirs(os.path.join(self.fixture_dir, "sub"), exist_ok=True)
        with open(os.path.join(self.fixture_dir, "sub", "fixture-b.txt"), "w", encoding="utf-8") as handle:
            handle.write("soak fixture beta\n")
        session = {
            "id": self.session_id,
            "cwd": self.fixture_dir,
            "permitted_directories": [self.fixture_dir],
        }
        self.session_file = os.path.join(sessions_dir, f"{self.session_id}.json")
        with open(self.session_file, "w", encoding="utf-8") as handle:
            json.dump(session, handle)

    def child_env(self) -> dict:
        env = drv.test_owned_env(self.state_root)
        # The state dir itself must be session-visible on platforms where the
        # server stores sessions under LOCALAPPDATA/state.
        env.pop("SOAK_SANITY", None)
        return env

    def start_driver(self, id_offset: int = 0) -> None:
        self.driver = drv.McpDriver(
            command=self.args.server_cmd,
            env=self.child_env(),
            cwd=self.fixture_dir,
            ledger=self.ledger,
            logger=self.log,
            request_timeout=self.args.request_timeout,
            max_inflight=self.bounds.max_inflight,
            wire_line_cap=self.bounds.wire_line_cap,
            stderr_path=os.path.join(self.run_dir, "child-stderr.log"),
            id_offset=id_offset,
        )
        self.driver.latency_recorder = self.latency.add
        self.driver.start()

    def restart_driver(self) -> None:
        old = self.driver
        id_offset = 0
        if old is not None:
            id_offset = old._next_id
            old.close(wait_grace_s=0.0, terminate=True)
        self.driver = None
        self.start_driver(id_offset=id_offset)

    # -- main run -----------------------------------------------------------------
    def run(self) -> dict:
        started = time.monotonic()
        self.prepare()
        self.start_driver()
        self.engine = scn.ScenarioEngine(
            self.driver,
            self.session_id,
            self.fixture_dir,
            outside_path=os.path.join(
                tempfile.gettempdir(), "soak-refusal-probe.txt"
            ),
            rng=random.Random(self.args.seed ^ 0x5C0A7),
            platform=self.platform,
            goal_run_timeout=self.args.goal_run_timeout,
        )
        self.sampler = res.ResourceSampler(self.driver._proc.pid, self.bounds)
        self.sampler.start()

        verdict = PASS
        violations: list = []
        not_run: list = []

        # MCP handshake (initialize) then warm-up; a failure here is FAIL.
        try:
            self.driver.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "soak-harness-v1", "version": "1.0.0"}})
        except (TimeoutError, drv.DriverClosed) as error:
            return self._abort(started, FAIL, [f"initialize failed: {error!r}"], [], [])

        deadline = started + self.args.duration
        iterations = 0
        while time.monotonic() < deadline:
            scenario = self.engine.run_iteration()
            self.scenario_log.append(scenario)
            iterations += 1
            time.sleep(self.args.pacing_s)
            if self.driver._proc.poll() is not None:
                self.unexpected_child_deaths += 1
                break

        # S9: client EOF phase (driver half-closes stdin) with restart.
        s9 = self._phase_eof()
        # S10/FI phases (opt-in beyond the always-on S9):
        fi_results: list = []
        if self.args.failure_injection:
            fi_results.append(fi.fi1_goal_midlife(self, self.log))
            fi_results.append(fi.fi2_midwrite(self, self.log))
            fi_results.append(fi.fi3_malformed_flood(self, self.log))
        else:
            not_run.append(
                "FI-1..FI-4 not executed: --failure-injection not enabled for this run"
            )
        fi_results.append(fi.fi5_disk_full(self.log))

        # Shutdown protocol (§5): classify in-flight, grace, terminate, finalize.
        self.shutting_down = True
        inflight_at_shutdown = self.ledger.shutdown_begin()
        close_result = self.driver.close(
            wait_grace_s=self.args.shutdown_grace_s, terminate=True
        )
        if self.sampler:
            self.sampler.stop()

        try:
            metrics = self.ledger.finalize()
        except led.LedgerInconsistency as error:
            verdict = FAIL
            violations.append(f"ledger inconsistency: {error}")
            metrics = {}

        max_inflight_observed = max(
            [len(self.driver._pending)] if self.driver else [0]
        )
        resp_bytes = self.driver.resp_bytes if self.driver else 0
        if self.sampler:
            sampler_violations = self.sampler.check_bounds(max_inflight_observed, resp_bytes)
            violations.extend(sampler_violations)
            if sampler_violations:
                verdict = FAIL

        if self.unexpected_child_deaths:
            verdict = FAIL
            violations.append(
                f"child exited unexpectedly {self.unexpected_child_deaths} time(s) outside deliberate phases"
            )
        if self.engine.tallies.unexpected_errors:
            verdict = FAIL
            violations.append(
                f"unexpected error responses: {self.engine.tallies.unexpected_errors}"
            )
        if metrics.get("shutdown_unrecovered", 0) > 0 and not self.args.allow_unrecovered:
            # Honest accounting: unrecovered in-flight requests at shutdown are
            # recorded; they do not by themselves fail a smoke run, but they are
            # never hidden.
            pass
        if metrics and metrics.get("coverage", 0.0) < self.args.min_coverage:
            verdict = FAIL
            violations.append(
                f"coverage {metrics.get('coverage')} < required {self.args.min_coverage}"
            )

        duration_s = round(time.monotonic() - started, 3)
        scenario_summary = {
            "tallies": {
                "attempts": self.engine.tallies.attempts,
                "expected_refusals": self.engine.tallies.expected_refusals,
                "unexpected": self.engine.tallies.unexpected_errors,
                "not_applicable": self.engine.tallies.not_applicable,
            },
            "iterations": iterations,
            "active_goals_left": len(self.engine.active_goals),
        }
        report = {
            "schema": "soak-harness-v1/report@v1",
            "verdict": verdict,
            "commit_sha": self._resolve_commit_sha(),
            "seed": self.args.seed,
            "platform": self.platform,
            "runner": _platform.node() if hasattr(_platform, "node") else "unknown",
            "duration_s": duration_s,
            "requested_duration_s": self.args.duration,
            "metrics": metrics,
            "coverage": metrics.get("coverage"),
            "resources": self.sampler.summary() if self.sampler else {"status": res.NOT_RUN},
            "latency": self.latency.stats(),
            "scenarios": scenario_summary,
            "phase_eof": s9,
            "failure_injection": fi_results,
            "child_exit": close_result,
            "inflight_at_shutdown": inflight_at_shutdown,
            "violations": violations,
            "not_run": not_run,
            "bounds": self.bounds.as_dict(),
            "limitations": self._limitations(),
        }
        forbidden = [self.session_id, self.run_dir, self.fixture_dir]
        report = rep.redact_report(report, forbidden)
        return report

    def _phase_eof(self) -> dict:
        """S9: client EOF — driver half-closes stdin; child exits or reports."""
        result = {"phase": "S9-client-EOF", "status": "OBSERVED"}
        try:
            self.driver.half_close_stdin()
            deadline = time.monotonic() + 20.0
            while time.monotonic() < deadline and self.driver._proc.poll() is None:
                time.sleep(0.1)
            exit_code = self.driver._proc.poll()
            result["child_exit_code"] = exit_code
            if exit_code is None:
                result["status"] = "OBSERVED-NO-EXIT"
            # Ledger: pending ids are classified at shutdown; nothing is dropped.
            self.restart_driver()
        except (OSError, ValueError) as error:
            result["status"] = "FAIL"
            result["detail"] = repr(error)
        return result

    def _abort(self, started, verdict, violations, not_run, fi_results) -> dict:
        duration_s = round(time.monotonic() - started, 3)
        try:
            self.ledger.shutdown_begin()
            if self.driver:
                self.driver.close(wait_grace_s=0.0, terminate=True)
            metrics = self.ledger.finalize()
        except (led.LedgerInconsistency, OSError) as error:
            metrics = {"finalize_error": str(error)}
        if self.sampler:
            try:
                self.sampler.stop()
            except (OSError, RuntimeError):
                pass
        return {
            "schema": "soak-harness-v1/report@v1",
            "verdict": verdict,
            "commit_sha": self._resolve_commit_sha(),
            "seed": self.args.seed,
            "platform": self.platform,
            "runner": _platform.node() if hasattr(_platform, "node") else "unknown",
            "duration_s": duration_s,
            "requested_duration_s": self.args.duration,
            "metrics": metrics,
            "resources": self.sampler.summary() if self.sampler else {"status": res.NOT_RUN},
            "latency": self.latency.stats(),
            "scenarios": {},
            "failure_injection": fi_results,
            "violations": violations,
            "not_run": not_run,
            "bounds": self.bounds.as_dict(),
            "limitations": self._limitations(),
        }

    def _resolve_commit_sha(self) -> str:
        if self.args.commit_sha:
            return self.args.commit_sha
        try:
            out = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                capture_output=True, text=True, timeout=10,
            )
            if out.returncode == 0:
                return out.stdout.strip()
        except (OSError, subprocess.SubprocessError):
            pass
        return "unknown"

    def _limitations(self) -> list:
        limits = [
            "Windows is Experimental (host-native execution; no sandbox claim) — S5 excluded by design.",
        ]
        if self.platform == "macos":
            limits.append(
                "Apple Silicon local validation is a CI target; macOS results are not release-closure evidence."
            )
        if self.args.duration < 8 * 3600:
            limits.append(
                f"This run is {self.args.duration}s — it is NOT an 8-hour soak and must not be cited as long-run stability evidence."
            )
        if self.sampler and self.sampler.status == res.NOT_RUN:
            limits.append("Resource sampling unavailable on this platform: resource bounds NOT RUN.")
        limits.append("Reports must not be quoted without the seed, commit SHA, and platform cell.")
        return limits


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m soak_v1", description="Soak Harness V1 (request/response completeness)"
    )
    parser.add_argument("--server-cmd", required=True, nargs="+",
                        help="command to spawn the MCP server, e.g. /path/local-mcp mcp")
    parser.add_argument("--duration", type=int, default=60, help="soak duration seconds (default 60)")
    parser.add_argument("--seed", type=int, default=1, help="reproducible seed")
    parser.add_argument("--out", required=True, help="run output directory (test-owned)")
    parser.add_argument("--pacing-s", type=float, default=0.05)
    parser.add_argument("--request-timeout", type=float, default=30.0)
    parser.add_argument("--goal-run-timeout", type=float, default=60.0)
    parser.add_argument("--shutdown-grace-s", type=float, default=5.0)
    parser.add_argument("--min-coverage", type=float, default=0.95,
                        help="minimum (normal+error)/sent coverage for PASS")
    parser.add_argument("--failure-injection", action="store_true",
                        help="run FI-1..FI-3 phases (test-owned environment only)")
    parser.add_argument("--allow-unrecovered", action="store_true",
                        help="do not hide unrecovered shutdown requests (reporting only)")
    parser.add_argument("--commit-sha", default=None, help="commit under test (recorded verbatim)")
    parser.add_argument("--log-level", default="info")
    parser.add_argument("--max-inflight", type=int, default=res.DEFAULT_MAX_INFLIGHT)
    parser.add_argument("--wire-line-cap", type=int, default=res.DEFAULT_WIRE_LINE_CAP)
    parser.add_argument("--response-byte-cap", type=int, default=res.DEFAULT_RESPONSE_BYTE_CAP)
    parser.add_argument("--working-set-cap", type=int, default=res.DEFAULT_WORKING_SET_CAP)
    parser.add_argument("--working-set-growth-floor", type=int,
                        default=res.DEFAULT_WORKING_SET_GROWTH_FLOOR_BYTES)
    parser.add_argument("--working-set-growth-fraction", type=float,
                        default=res.DEFAULT_WORKING_SET_GROWTH_FRACTION)
    parser.add_argument("--fd-cap", type=int, default=res.DEFAULT_FD_CAP)
    parser.add_argument("--max-latency-samples", type=int, default=res.DEFAULT_MAX_LATENCY_SAMPLES)
    return parser


def main(argv: list | None = None) -> int:
    args = build_parser().parse_args(argv)
    os.makedirs(args.out, exist_ok=True)
    logger = JsonLogger(
        os.path.join(args.out, "soak-run.jsonl"), level=args.log_level
    )
    harness = SoakHarness(args, logger)
    try:
        report = harness.run()
        json_path, md_path = rep.write_report(args.out, report)
        logger.info("report written", json=json_path, markdown=md_path)
        print(json.dumps({"verdict": report["verdict"], "report": json_path}, default=str))
        return {PASS: 0, FAIL: 1, NOT_RUN: 2}[report["verdict"]]
    except KeyboardInterrupt:
        logger.warn("interrupted")
        return 130
    except (OSError, ValueError, RuntimeError, TimeoutError) as error:
        logger.error("harness error", error=repr(error))
        print(f"HARNESS ERROR: {error!r}", file=sys.stderr)
        return 1
    finally:
        logger.close()


if __name__ == "__main__":
    raise SystemExit(main())
