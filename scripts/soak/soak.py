#!/usr/bin/env python3
"""Bounded-memory JSON-RPC stdio soak harness."""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import os
import pathlib
import random
import statistics
import subprocess
import tempfile
import threading
import time
import uuid

MAX_SLOW_EVENTS = 5000
MAX_STDERR_LINES = 40
MAX_INFLIGHT = 128
MAX_REQUESTS = 5_000_000
MAX_RESPONSE_LINE_BYTES = 16 * 1024 * 1024


def _child_working_set_kb(pid: int) -> int | None:
    if os.name != "nt":
        try:
            pages = pathlib.Path(f"/proc/{pid}/statm").read_text().split()[1]
            return int(pages) * os.sysconf("SC_PAGE_SIZE") // 1024
        except (OSError, ValueError, IndexError):
            return None
    try:
        import ctypes
        from ctypes import wintypes

        class PMC(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        handle = ctypes.windll.kernel32.OpenProcess(0x1000, False, pid)
        if not handle:
            return None
        try:
            info = PMC()
            info.cb = ctypes.sizeof(PMC)
            if ctypes.windll.psapi.GetProcessMemoryInfo(handle, ctypes.byref(info), info.cb):
                return info.WorkingSetSize // 1024
        finally:
            ctypes.windll.kernel32.CloseHandle(handle)
    except Exception:
        return None
    return None


def _percentile(samples: list[float], percentile: float) -> float | None:
    if not samples:
        return None
    ordered = sorted(samples)
    return ordered[min(int(percentile * len(ordered)), len(ordered) - 1)]


class ResponseLedger:
    """Counts protocol events without retaining response payloads or all IDs."""

    def __init__(self, max_requests: int, request_timeout: float):
        self.lock = threading.Lock()
        self.max_requests = max_requests
        self.request_timeout = request_timeout
        self.sent = bytearray()
        self.responded = bytearray()
        self.timed_out = bytearray()
        self.pending: dict[int, float] = {}
        self.latencies_ms: list[float] = []
        self.latency_observations = 0
        self.latency_rng = random.Random(0)
        self.attempted = 0
        self.send_succeeded = 0
        self.send_failed = 0
        self.normal_responses = 0
        self.jsonrpc_errors = 0
        self.invalid_responses = 0
        self.invalid_json = 0
        self.duplicate_responses = 0
        self.unknown_ids = 0
        self.timeouts = 0
        self.shutdown_recovered = 0
        self.response_count = 0
        self.bytes_read = 0
        self.slow_events: collections.deque[dict] = collections.deque(maxlen=MAX_SLOW_EVENTS)
        self.shutdown_started = False
        self.start = time.monotonic()

    def begin_attempt(self, request_id: int) -> float:
        now = time.monotonic()
        with self.lock:
            self.attempted += 1
            self.sent.append(0)
            self.responded.append(0)
            self.timed_out.append(0)
            self.pending[request_id] = now
        return now

    def mark_sent(self, request_id: int) -> None:
        with self.lock:
            if not self.sent[request_id - 1]:
                self.sent[request_id - 1] = 1
                self.send_succeeded += 1

    def mark_send_failed(self, request_id: int) -> None:
        with self.lock:
            if self.sent[request_id - 1] == 0:
                self.sent[request_id - 1] = 2
                self.send_failed += 1
            self.pending.pop(request_id, None)

    def expire_requests(self) -> None:
        now = time.monotonic()
        with self.lock:
            expired = [rid for rid, sent_at in self.pending.items()
                       if now - sent_at >= self.request_timeout]
            for rid in expired:
                self.pending.pop(rid, None)
                self.timed_out[rid - 1] = 1
                self.timeouts += 1

    def record_invalid_line(self, byte_count: int) -> None:
        with self.lock:
            self.bytes_read += byte_count
            self.invalid_json += 1

    def receive_line(self, raw: bytes) -> None:
        with self.lock:
            self.bytes_read += len(raw)
        try:
            obj = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            with self.lock:
                self.invalid_json += 1
            return
        if not isinstance(obj, dict) or obj.get("jsonrpc") != "2.0" or "id" not in obj:
            with self.lock:
                self.invalid_responses += 1
            return
        request_id = obj["id"]
        if isinstance(request_id, bool) or not isinstance(request_id, int) or request_id < 1:
            with self.lock:
                self.invalid_responses += 1
            return
        has_result = "result" in obj
        has_error = "error" in obj
        if has_result == has_error:
            with self.lock:
                self.invalid_responses += 1
            return
        now = time.monotonic()
        with self.lock:
            if request_id > len(self.sent) or self.sent[request_id - 1] != 1:
                self.unknown_ids += 1
                return
            if self.responded[request_id - 1]:
                self.duplicate_responses += 1
                return
            self.responded[request_id - 1] = 1
            sent_at = self.pending.pop(request_id, None)
            self.response_count += 1
            if has_error:
                self.jsonrpc_errors += 1
            else:
                self.normal_responses += 1
            if self.shutdown_started:
                self.shutdown_recovered += 1
            if sent_at is not None:
                latency_ms = (now - sent_at) * 1000
                self.latency_observations += 1
                if len(self.latencies_ms) < 100_000:
                    self.latencies_ms.append(latency_ms)
                else:
                    slot = self.latency_rng.randrange(self.latency_observations)
                    if slot < len(self.latencies_ms):
                        self.latencies_ms[slot] = latency_ms
                if latency_ms >= 100:
                    self.slow_events.append({
                        "elapsed_sec": round(now - self.start, 3),
                        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
                        "latency_ms": round(latency_ms, 1),
                        "id": request_id,
                    })

    def begin_shutdown(self) -> int:
        with self.lock:
            self.shutdown_started = True
            return len(self.pending)

    def snapshot(self, extra: dict | None = None) -> dict:
        with self.lock:
            sent = self.send_succeeded
            result = {
                "requests_attempted": self.attempted,
                "requests_send_succeeded": sent,
                "requests_send_failed": self.send_failed,
                "normal_responses": self.normal_responses,
                "jsonrpc_error_responses": self.jsonrpc_errors,
                "invalid_responses": self.invalid_responses,
                "invalid_json": self.invalid_json,
                "duplicate_responses": self.duplicate_responses,
                "unknown_id_responses": self.unknown_ids,
                "response_count": self.response_count,
                "timeouts": self.timeouts,
                "shutdown_inflight": getattr(self, "shutdown_inflight", None),
                "shutdown_recovered_responses": self.shutdown_recovered,
                "final_inflight": len(self.pending),
                "final_unanswered": sent - self.response_count,
                "success_rate": self.normal_responses / sent if sent else None,
                "fully_responded": sent > 0 and self.response_count == sent
                and self.invalid_responses == 0 and self.invalid_json == 0
                and self.duplicate_responses == 0 and self.unknown_ids == 0
                and not self.pending,
                "all_requests_succeeded": sent > 0 and self.normal_responses == sent
                and self.jsonrpc_errors == 0 and self.invalid_responses == 0
                and self.invalid_json == 0 and self.duplicate_responses == 0
                and self.unknown_ids == 0 and not self.pending,
                "response_bytes_read": self.bytes_read,
                "latency_observation_count": self.latency_observations,
                "latency_sample_count": len(self.latencies_ms),
                "latency_percentiles_are_reservoir_estimates": self.latency_observations > len(self.latencies_ms),
                "p50_ms": statistics.median(self.latencies_ms) if self.latencies_ms else None,
                "p95_ms": _percentile(self.latencies_ms, 0.95),
                "p99_ms": _percentile(self.latencies_ms, 0.99),
                "max_ms": max(self.latencies_ms, default=None),
                "slow_events": list(self.slow_events),
                "client_id_tracking_bytes": len(self.sent) + len(self.responded) + len(self.timed_out),
            }
        if extra:
            result.update(extra)
        return result


def _atomic_json_write(path: pathlib.Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    except BaseException:
        try:
            os.unlink(name)
        except OSError:
            pass
        raise


def _prepare_state(state: pathlib.Path) -> tuple[str, pathlib.Path]:
    state.mkdir(parents=True, exist_ok=False)
    sessions_dir = state / "local-mcp" / "sessions"
    sessions_dir.mkdir(parents=True)
    cwd = state / "cwd"
    cwd.mkdir()
    session_id = str(uuid.uuid4())
    (sessions_dir / f"{session_id}.json").write_text(json.dumps({
        "id": session_id,
        "cwd": str(cwd),
        "permitted_directories": [str(cwd)],
    }), encoding="utf-8")
    return session_id, cwd


def run(binary: str | list[str], duration: float, concurrency: int, state: pathlib.Path,
        output: pathlib.Path, request_timeout: float, shutdown_timeout: float,
        max_requests: int) -> dict:
    if not 1 <= concurrency <= MAX_INFLIGHT:
        raise ValueError(f"concurrency must be between 1 and {MAX_INFLIGHT}")
    if not 1 <= max_requests <= MAX_REQUESTS:
        raise ValueError(f"max_requests must be between 1 and {MAX_REQUESTS}")
    if duration <= 0 or request_timeout <= 0 or shutdown_timeout <= 0:
        raise ValueError("duration and timeouts must be positive")
    if state.exists():
        raise FileExistsError(f"refusing to overwrite existing state directory: {state}")
    if output.exists():
        raise FileExistsError(f"refusing to overwrite existing result file: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump({"run_state": "initializing"}, stream)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    _prepare_state(state)
    env = os.environ.copy()
    env["LOCALAPPDATA"] = str(state)
    env["APPDATA"] = str(state)
    command = [binary, "mcp"] if isinstance(binary, str) else [*binary, "mcp"]
    proc = subprocess.Popen(command, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            bufsize=0, env=env)
    ledger = ResponseLedger(max_requests, request_timeout)
    stderr_tail: collections.deque[str] = collections.deque(maxlen=MAX_STDERR_LINES)
    eof = threading.Event()
    reader_errors: list[str] = []
    mem_samples: list[list[float | int]] = []
    started = time.monotonic()
    start_utc = dt.datetime.now(dt.timezone.utc).isoformat()
    send_error: str | None = None

    def reader() -> None:
        try:
            assert proc.stdout is not None
            while True:
                line = proc.stdout.readline(MAX_RESPONSE_LINE_BYTES + 1)
                if not line:
                    break
                if len(line) > MAX_RESPONSE_LINE_BYTES:
                    consumed = len(line)
                    while line and not line.endswith(b"\n"):
                        line = proc.stdout.readline(MAX_RESPONSE_LINE_BYTES + 1)
                        consumed += len(line)
                    ledger.record_invalid_line(consumed)
                else:
                    ledger.receive_line(line)
        except Exception as exc:
            reader_errors.append(repr(exc))
        finally:
            eof.set()

    def read_stderr() -> None:
        assert proc.stderr is not None
        while True:
            line = proc.stderr.readline(8192)
            if not line:
                break
            stderr_tail.append(line.decode("utf-8", errors="replace").rstrip())

    reader_thread = threading.Thread(target=reader, name="soak-stdout-reader", daemon=True)
    stderr_thread = threading.Thread(target=read_stderr, name="soak-stderr-reader", daemon=True)
    reader_thread.start()
    stderr_thread.start()

    checkpoint_stop = threading.Event()
    checkpoint_errors: list[str] = []

    def checkpoint() -> None:
        while not checkpoint_stop.wait(30):
            try:
                _atomic_json_write(output, ledger.snapshot({
                    "run_state": "running",
                    "requested_duration_sec": duration,
                    "elapsed_sec": round(time.monotonic() - started, 3),
                    "start_time_utc": start_utc,
                }))
            except OSError as exc:
                checkpoint_errors.append(repr(exc))

    checkpoint_thread = threading.Thread(target=checkpoint, name="soak-checkpoint", daemon=True)
    checkpoint_thread.start()
    monitor_stop = threading.Event()

    def monitor_memory() -> None:
        while not monitor_stop.is_set() and proc.poll() is None:
            value = _child_working_set_kb(proc.pid)
            if value is not None:
                mem_samples.append([round(time.monotonic() - started, 1), value])
            monitor_stop.wait(60)

    memory_thread = threading.Thread(target=monitor_memory, name="soak-memory-monitor", daemon=True)
    memory_thread.start()
    methods = ("ping", "tools/list")
    next_id = 1
    deadline = started + duration
    rng = random.Random(0)
    full_duration = True
    child_exited_during_run = False
    shutdown_inflight = 0
    forced_kill = False
    try:
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                full_duration = False
                child_exited_during_run = True
                break
            ledger.expire_requests()
            with ledger.lock:
                available = concurrency - len(ledger.pending)
            if available <= 0:
                time.sleep(0.005)
                continue
            if next_id > max_requests:
                break
            request_id = next_id
            next_id += 1
            ledger.begin_attempt(request_id)
            try:
                assert proc.stdin is not None
                payload = json.dumps({"jsonrpc": "2.0", "id": request_id,
                                      "method": rng.choice(methods)}, separators=(",", ":"))
                # Keep the reader from classifying a fast response before send success is recorded.
                with ledger.lock:
                    proc.stdin.write((payload + "\n").encode("utf-8"))
                    proc.stdin.flush()
                    ledger.sent[request_id - 1] = 1
                    ledger.send_succeeded += 1
            except (BrokenPipeError, OSError, ValueError) as exc:
                ledger.mark_send_failed(request_id)
                send_error = repr(exc)
                full_duration = False
                break
        if time.monotonic() < deadline and next_id > max_requests:
            full_duration = False
    except BaseException as exc:
        send_error = repr(exc)
        full_duration = False
    finally:
        ledger.expire_requests()
        shutdown_started = time.monotonic()
        shutdown_inflight = ledger.begin_shutdown()
        ledger.shutdown_inflight = shutdown_inflight
        try:
            if proc.stdin is not None:
                proc.stdin.close()
        except (OSError, ValueError):
            pass
        try:
            proc.wait(timeout=shutdown_timeout)
        except subprocess.TimeoutExpired:
            forced_kill = True
            proc.kill()
            proc.wait()
        reader_thread.join(timeout=shutdown_timeout)
        stderr_thread.join(timeout=shutdown_timeout)
        if proc.stdout is not None:
            proc.stdout.close()
        if proc.stderr is not None:
            proc.stderr.close()
        if reader_thread.is_alive() or stderr_thread.is_alive():
            reader_errors.append("reader did not drain to EOF before shutdown timeout")
        monitor_stop.set()
        memory_thread.join(timeout=2)
        checkpoint_stop.set()
        checkpoint_thread.join(timeout=2)

    elapsed = time.monotonic() - started
    extra = {
        "requested_duration_sec": duration,
        "elapsed_sec": round(elapsed, 3),
        "completed_full_duration": full_duration and elapsed >= duration - 2,
        "start_time_utc": start_utc,
        "end_time_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "shutdown_timeout_sec": shutdown_timeout,
        "shutdown_timed_out": forced_kill,
        "shutdown_elapsed_sec": round(time.monotonic() - shutdown_started, 3),
        "child_returncode": proc.returncode,
        "normal_exit": proc.returncode == 0 and not forced_kill,
        "child_exited_early": child_exited_during_run,
        "send_error": send_error,
        "reader_errors": reader_errors,
        "stdout_eof": eof.is_set(),
        "checkpoint_errors": checkpoint_errors,
        "stderr_tail": list(stderr_tail),
        "server_working_set_samples_sec_kb": mem_samples,
        "request_timeout_sec": request_timeout,
        "max_inflight": concurrency,
        "max_requests": max_requests,
    }
    summary = ledger.snapshot(extra)
    _atomic_json_write(output, summary)
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--duration", type=float, default=300)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--state-dir", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--request-timeout", type=float, default=30)
    parser.add_argument("--shutdown-timeout", type=float, default=10)
    parser.add_argument("--max-requests", type=int, default=MAX_REQUESTS)
    args = parser.parse_args()
    result = run(args.binary, args.duration, args.concurrency,
                 pathlib.Path(args.state_dir), pathlib.Path(args.out),
                 args.request_timeout, args.shutdown_timeout, args.max_requests)
    print(json.dumps(result, indent=2))
    return 0 if result["child_returncode"] == 0 and not result["shutdown_timed_out"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
