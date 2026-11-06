"""Seeded weighted scenario engine (Soak Harness V1, Slice 3).

Scenarios S1..S10 from docs/SOAK_HARNESS_V1_DESIGN.md §4. Windows records S5
as ``not-applicable (platform experimental)`` — never as a sandbox result.
Every request goes through the driver and is accounted in the ledger.
"""

from __future__ import annotations

import json
import random
import time

from .driver import DriverClosed

EXECUTABLE_SCENARIOS = ("S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8")
SUPPORTED_WEIGHTS = {
    "S1": 40,
    "S2": 10,
    "S3": 15,
    "S4": 5,
    "S5": 10,
    "S6": 10,
    "S7": 5,
    "S8": 1,
}
MAX_CONCURRENT_GOALS = 20


class ScenarioTallies:
    def __init__(self) -> None:
        self.attempts: dict = {}
        self.expected_refusals: dict = {}
        self.unexpected_errors: dict = {}
        self.not_applicable: dict = {}

    def note(self, scenario: str, bucket: str) -> None:
        store = {
            "attempt": self.attempts,
            "refusal": self.expected_refusals,
            "unexpected": self.unexpected_errors,
            "na": self.not_applicable,
        }[bucket]
        store[scenario] = store.get(scenario, 0) + 1


class ScenarioEngine:
    def __init__(
        self,
        driver,
        session_id: str,
        fixture_dir: str,
        outside_path: str,
        rng: random.Random,
        platform: str,
        goal_run_timeout: float = 60.0,
    ) -> None:
        self.driver = driver
        self.session_id = session_id
        self.fixture_dir = fixture_dir
        self.outside_path = outside_path
        self.rng = rng
        self.platform = platform  # "windows" | "linux" | "macos" | other
        self.goal_run_timeout = goal_run_timeout
        self.tallies = ScenarioTallies()
        self.active_goals: list = []
        self.child_alive_callback = lambda: True
        self.scenario_errors: list = []

    # -- weighted selection ----------------------------------------------------
    def pick(self) -> str:
        population = []
        for name, weight in SUPPORTED_WEIGHTS.items():
            if name == "S5" and self.platform == "windows":
                continue  # Windows: S5 excluded per Experimental status
            population.extend([name] * weight)
        return self.rng.choice(population)

    def run_iteration(self) -> str:
        scenario = self.pick()
        handler = getattr(self, f"s{scenario[1:]}")
        try:
            handler()
        except (TimeoutError, ConnectionError, BrokenPipeError, OSError, ValueError, DriverClosed) as error:
            self.tallies.note(scenario, "unexpected")
            self.scenario_errors.append({"scenario": scenario, "error": repr(error)})
        self.tallies.note(scenario, "attempt")
        return scenario

    # -- helpers ----------------------------------------------------------------
    def call(self, name: str, params: dict, timeout: float | None = None):
        response = self.driver.request(
            "tools/call",
            {"name": name, "arguments": params},
            timeout=timeout,
        )
        return response

    def record_unexpected(self, scenario: str, response) -> None:
        self.tallies.note(scenario, "unexpected")
        self.scenario_errors.append(
            {"scenario": scenario, "error": f"unexpected error response: {response.payload}"[:400]}
        )

    # -- S1: read file / list directory -----------------------------------------
    def s1(self) -> None:
        if self.rng.random() < 0.5:
            self.call("read_file", {"session_id": self.session_id, "path": "fixture-a.txt"})
        else:
            self.call("list_directory", {"session_id": self.session_id, "path": "."})

    # -- S2: session info ---------------------------------------------------------
    def s2(self) -> None:
        self.call("session_info", {"session_id": self.session_id})

    # -- S3: goal start / status / cancel ----------------------------------------
    # Product fact: the durable Goal model allows ONE non-terminal Goal per
    # session. The engine therefore rotates a single goal; goal_start while a
    # goal is active (or creation-attempt caps are exhausted) is an expected
    # refusal, never an unexpected error. (Design §4's "≤ 20 concurrent
    # goals" predates the V1 one-non-terminal-goal rule — reconciled here;
    # flagged in the implementation README.)
    def s3(self) -> None:
        if self.active_goals:
            goal_id = self.active_goals[0]
            if self.rng.random() < 0.5:
                self.call("goal_status", {"session_id": self.session_id, "goal_id": goal_id})
            else:
                response = self.call(
                    "goal_cancel",
                    {"session_id": self.session_id, "goal_id": goal_id, "reason": "soak rotation"},
                )
                if response.is_error:
                    self.tallies.note("S3", "refusal")
                else:
                    self.active_goals.pop(0)
            return
        response = self.call(
            "goal_start",
            {
                "session_id": self.session_id,
                "objective": "soak scenario S3 bounded goal lifecycle",
                "title": "soak-s3",
                "completion_criteria": ["soak harness cancels this goal"],
            },
        )
        if response.is_error:
            # Creation refused (e.g., the durable creation-attempt cap): an
            # expected, classified observation — never treated as success.
            self.tallies.note("S3", "refusal")
            return
        goal_id = self._extract_goal_id(response)
        if goal_id is None:
            self.tallies.note("S3", "unexpected")
            self.scenario_errors.append({"scenario": "S3", "error": "no goal id in result"})
            return
        self.active_goals.append(goal_id)
        self.call("goal_status", {"session_id": self.session_id, "goal_id": goal_id})

    def _extract_goal_id(self, response) -> str | None:
        try:
            content = response.payload["result"]["content"][0]["text"]
            parsed = json.loads(content) if isinstance(content, str) else content
            if isinstance(parsed, dict):
                goal = parsed.get("goal") or parsed
                return goal.get("id") or goal.get("goal_id")
        except (KeyError, IndexError, TypeError, ValueError):
            pass
        return None

    # -- S4: bounded goal run ------------------------------------------------------
    def s4(self) -> None:
        if not self.active_goals:
            return
        goal_id = self.rng.choice(self.active_goals)
        # Without Codex CLI the run must produce a recorded, non-hanging
        # failure. A timeout budget is applied; the ledger classifies the
        # outcome either way.
        self.call(
            "goal_run",
            {"session_id": self.session_id, "goal_id": goal_id, "max_steps": self.rng.choice((1, 2))},
            timeout=self.goal_run_timeout,
        )

    # -- S5: sandboxed foreground execute (Linux/macOS only) -----------------------
    def s5(self) -> None:
        if self.platform == "windows":
            # S5 is excluded on Windows per Experimental status; this path is
            # unreachable via pick() and is kept as an explicit guard so a
            # Windows run can never record a sandbox execution.
            self.tallies.note("S5", "na")
            return
        self.call(
            "execute",
            {"session_id": self.session_id, "command": ["echo", "soak-s5"]},
            timeout=60.0,
        )

    # -- S6: refusal path ------------------------------------------------------------
    def s6(self) -> None:
        response = self.call(
            "write_file",
            {"session_id": self.session_id, "path": self.outside_path, "content": "soak refusal probe"},
        )
        if response.is_error:
            self.tallies.note("S6", "refusal")
        else:
            # Zero silent truncation: an unexpected success outside the
            # permitted roots is a finding, not a pass.
            self.record_unexpected("S6", response)

    # -- S7: malformed JSON-RPC ---------------------------------------------------------
    def s7(self) -> None:
        variant = self.rng.randrange(3)
        if variant == 0:
            self.driver.send_raw("this is not json\n")
        elif variant == 1:
            # Wrong protocol version with a raw-tracked id (<= 0): the answer
            # is counted in raw_probe_responses, never as an unknown id.
            self.driver.send_raw(
                json.dumps({"jsonrpc": "1.0", "id": -1, "method": "ping"}) + "\n"
            )
        else:
            self.driver.send_notification("tools/call", {"name": "session_info"})
        # Interleaved probe: the child must remain responsive within the
        # latency bound (FI-3 acceptance for flood conditions).
        self.call("session_info", {"session_id": self.session_id}, timeout=30.0)

    # -- S8: oversized frame ---------------------------------------------------------------
    def s8(self) -> None:
        request_id = self.driver.new_id()
        self.driver.ledger.attempt(request_id)
        frame = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
             "params": {"name": "session_info", "arguments": {"session_id": self.session_id,
                        "pad": "x" * (self.driver.wire_line_cap_hint() + 1)}}}
        )
        try:
            self.driver.send_raw(frame)
            self.driver.ledger.sent(request_id)
            # The oversized frame is answered (error) or not (timeout); both
            # are classified by the ledger via a wait-and-classify helper.
            self._wait_and_classify(request_id)
        except (BrokenPipeError, OSError) as error:
            self.driver.ledger.send_failed(request_id, repr(error))

    def _wait_and_classify(self, request_id, timeout: float = 30.0) -> None:
        deadline = time.monotonic() + timeout
        nulls_before = self.driver.ledger.null_id_count()
        while time.monotonic() < deadline:
            state = self.driver.ledger.snapshot_state(request_id)
            if state not in ("ATTEMPTED", "SENT"):
                return
            if self.driver.ledger.null_id_count() > nulls_before:
                # The product refuses oversized request frames without
                # parsing them: the refusal answer carries id:null. The
                # refusal is the response; record it against this id.
                self.driver.ledger.respond_refused(
                    request_id, "refused: oversized frame answered with id-null resource-limit error"
                )
                return
            time.sleep(0.1)
        self.driver.ledger.timed_out(request_id, f"no response within {timeout}s (oversized frame)")
