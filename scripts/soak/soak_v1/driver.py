"""MCP stdio protocol driver (Soak Harness V1, Slice 1).

Spawns the server under test, speaks newline-delimited JSON-RPC over stdio,
and feeds every inbound/outbound event into the Response Ledger. Uses only
the Python standard library. The child is spawned with a test-owned
environment (state directories redirected) and never touches operator state.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass

from . import ledger as led


@dataclass
class Response:
    request_id: object
    payload: dict
    is_error: bool
    latency_s: float


class ProtocolViolation(Exception):
    """Child emitted a frame that cannot be attributed to any request."""


class DriverClosed(Exception):
    """The driver is not accepting requests (closed or never started)."""


class McpDriver:
    def __init__(
        self,
        command: list,
        env: dict,
        cwd: str,
        ledger: led.ResponseLedger,
        logger,
        request_timeout: float = 30.0,
        max_inflight: int = 128,
        write_timeout: float = 30.0,
        wire_line_cap: int = 8 * 1024 * 1024,
        stderr_path: str | None = None,
        id_offset: int = 0,
    ) -> None:
        self.command = list(command)
        self.env = dict(env)
        self.cwd = cwd
        self.ledger = ledger
        self.log = logger
        self.request_timeout = request_timeout
        self.max_inflight = max_inflight
        self.write_timeout = write_timeout
        # Product request-frame cap (src/resource_limits.rs
        # MAX_MCP_REQUEST_FRAME_BYTES = 8 MiB). Oversized-frame probes exceed
        # this; the 16 MiB design figure is the response-frame cap and the
        # harness's own inbound line bound.
        self.wire_line_cap = wire_line_cap
        self._stderr_path = stderr_path
        self._stderr_handle = None
        # Request ids are unique across the whole run, including restarts: a
        # restarted driver continues the shared id space of the ledger.
        self._next_id = id_offset
        self.latency_recorder = None  # optional callable(latency_s)

        self._proc: subprocess.Popen | None = None
        self._reader: threading.Thread | None = None
        self._stderr_thread: threading.Thread | None = None
        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._pending: dict = {}  # id -> {"event": Event, "response": Response|None, "deadline": float}
        self._closed = False
        self._stdout_fd = None
        self.resp_bytes = 0

    # -- lifecycle -----------------------------------------------------------
    def start(self) -> None:
        if self._proc is not None:
            raise DriverClosed("driver already started")
        if self._stderr_path:
            self._stderr_handle = open(self._stderr_path, "ab")
            stderr_arg = self._stderr_handle
        else:
            stderr_arg = subprocess.DEVNULL
        self._proc = subprocess.Popen(
            self.command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr_arg,
            cwd=self.cwd,
            env=self.env,
            bufsize=0,
        )
        self._stdout_fd = self._proc.stdout
        self._reader = threading.Thread(
            target=self._read_loop, name="soak-reader", daemon=True
        )
        self._reader.start()

    def restart(self) -> None:
        """Restart the child after a termination/EOF phase."""
        self.close(wait_grace_s=0.0, terminate=True)
        self._proc = None
        self._reader = None
        self._stdout_fd = None
        self._closed = False
        self.start()

    def new_id(self) -> int:
        return self._allocate_id()

    def wire_line_cap_hint(self) -> int:
        return max(1024, self.wire_line_cap - 1024)

    # -- request path --------------------------------------------------------
    def _allocate_id(self) -> int:
        with self._cond:
            self._next_id += 1
            return self._next_id

    def request(
        self, method: str, params: dict | None, timeout: float | None = None
    ) -> Response:
        """Send one request and wait for its response.

        The ledger records ATTEMPTED -> SENT, then exactly one of RESPONDED /
        JSON-RPC ERROR / TIMED OUT / SEND FAILED / INVALID FRAME. Raises
        DriverClosed only before the frame could be handed to the pipe.
        """
        if self._closed or self._proc is None or self._proc.stdout is None:
            raise DriverClosed("driver is closed")
        timeout = self.request_timeout if timeout is None else timeout
        request_id = self._allocate_id()
        self.ledger.attempt(request_id)
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}}
        self._acquire_inflight_slot()
        payload = json.dumps(frame, separators=(",", ":")) + "\n"
        entry = {"event": threading.Event(), "response": None, "deadline": None}
        with self._cond:
            self._pending[request_id] = entry
        try:
            try:
                sent_time = time.monotonic()
                self._write_line(payload)
            except (BrokenPipeError, OSError, ValueError) as error:
                with self._cond:
                    self._pending.pop(request_id, None)
                self.ledger.send_failed(request_id, repr(error))
                raise DriverClosed(f"send failed: {error!r}") from error
            try:
                self.ledger.sent(request_id)  # no-op if a racing response classified it first
            except led.LedgerInconsistency:
                pass
            with self._cond:
                entry["deadline"] = time.monotonic() + timeout
                self._cond.notify_all()
            if entry["event"].wait(timeout):
                response = entry["response"]
                if response is None:
                    # Event fired via EOF path: the id stays open for the
                    # ledger to classify at shutdown/finalize.
                    raise DriverClosed(f"id {request_id}: stream ended without a response")
                response.latency_s = time.monotonic() - sent_time
                if self.latency_recorder:
                    try:
                        self.latency_recorder(response.latency_s)
                    except (TypeError, ValueError, RuntimeError):
                        pass
                return response
            self.ledger.timed_out(request_id, f"no response within {timeout}s")
            raise TimeoutError(f"id {request_id}: no response within {timeout}s")
        finally:
            with self._cond:
                self._pending.pop(request_id, None)
                self._cond.notify_all()

    def _acquire_inflight_slot(self) -> None:
        with self._cond:
            deadline = time.monotonic() + self.request_timeout
            while len(self._pending) >= self.max_inflight:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("in-flight bound starved the request queue")
                self._cond.wait(remaining)
            if len(self._pending) >= self.max_inflight:
                raise TimeoutError("in-flight bound starved the request queue")

    def inflight(self) -> int:
        with self._lock:
            return len(self._pending)

    def _write_line(self, payload: str) -> None:
        stream = self._proc.stdin
        data = payload.encode("utf-8")
        end = time.monotonic() + self.write_timeout
        view = memoryview(data)
        # Bounded write loop: a stuck child must not wedge the driver.
        while len(view):
            if time.monotonic() > end:
                raise TimeoutError("write deadline exceeded (child not draining)")
            written = stream.write(view)
            if not written:
                time.sleep(0.001)
                continue
            view = view[written:]
        try:
            stream.flush()
        except (BlockingIOError, OSError):
            if stream.closed:
                raise
        self.log.debug("wrote frame", bytes=len(data))

    # -- response path (reader thread) ---------------------------------------
    def _read_loop(self) -> None:
        proc = self._proc
        try:
            for raw in proc.stdout:
                self.resp_bytes += len(raw)
                self._handle_line(raw)
        except (OSError, ValueError):
            pass
        finally:
            # EOF: resolve pending waiters as invalid-frame closes so no
            # request blocks forever; the ledger classifies them in
            # finalize() according to their recorded state.
            with self._cond:
                for request_id, entry in list(self._pending.items()):
                    entry["response"] = None
                    entry["event"].set()
                self._cond.notify_all()

    def _handle_line(self, raw: bytes) -> None:
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as error:
            self.ledger.malformed_output()
            self.log.warn("inbound frame not UTF-8", detail=repr(error))
            return
        text = text.strip()
        if not text:
            return
        try:
            frame = json.loads(text)
        except json.JSONDecodeError as error:
            self.ledger.malformed_output()
            self.log.warn("inbound frame not JSON", detail=str(error.msg))
            return
        if not isinstance(frame, dict):
            self.ledger.malformed_output()
            self.log.warn("inbound frame not an object")
            return
        frame_id = frame.get("id", "__missing__")
        if frame_id == "__missing__" or frame_id is None:
            self.ledger.null_id_response()
            self.log.info("null/missing id frame (malformed-request answer or event)")
            return
        error = frame.get("error")
        is_error = error is not None
        response = Response(
            request_id=frame_id,
            payload=frame,
            is_error=is_error,
            latency_s=0.0,
        )
        self._deliver(response)

    def _deliver(self, response: Response) -> None:
        with self._cond:
            entry = self._pending.get(response.request_id)
            if entry is None:
                # Not currently awaited: a late response after timeout, a
                # duplicate, an unknown id, or a raw-probe id (<= 0).
                if isinstance(response.request_id, int) and response.request_id <= 0:
                    self.ledger.raw_probe_response()
                else:
                    self.log.info(
                        "unattributed response",
                        id=response.request_id,
                        is_error=response.is_error,
                    )
                    self.ledger.responded(response.request_id, is_error=response.is_error)
                return
            entry["response"] = response
            self.ledger.responded(response.request_id, is_error=response.is_error)
            entry["event"].set()
            self._cond.notify_all()

    # -- raw frames (failure injection / malformed scenarios) -----------------
    def send_raw(self, raw_line: str) -> None:
        """Send a raw line without ledger id tracking (S7/S8)."""
        if self._proc is None or self._closed:
            raise DriverClosed("driver is closed")
        self._write_line(raw_line if raw_line.endswith("\n") else raw_line + "\n")

    def send_notification(self, method: str, params: dict | None = None) -> None:
        """Send a notification (no id): no response by protocol design."""
        if self._closed or self._proc is None:
            raise DriverClosed("driver is closed")
        frame = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        self._write_line(json.dumps(frame, separators=(",", ":")) + "\n")
        self.ledger.notification_sent()

    # -- shutdown / EOF -------------------------------------------------------
    def half_close_stdin(self) -> None:
        """Client EOF (S9/FI-4): close stdin, keep reading stdout."""
        stream = self._proc.stdin
        if not stream.closed:
            try:
                stream.close()
            except OSError:
                pass

    def close(self, wait_grace_s: float = 0.0, terminate: bool = True) -> dict:
        """Shutdown protocol (§5): classify in-flight, wait for grace, reap child."""
        result = {"exit_code": None, "grace_waited_s": wait_grace_s, "terminated": False}
        if self._proc is None:
            return result
        self._closed = True
        self.half_close_stdin()
        if wait_grace_s > 0:
            deadline = time.monotonic() + wait_grace_s
            while time.monotonic() < deadline:
                if self._proc.poll() is not None:
                    break
                time.sleep(0.05)
        if self._proc.poll() is None and terminate:
            try:
                self._proc.terminate()
            except OSError:
                pass
            result["terminated"] = True
            try:
                self._proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._proc.kill()
                self._proc.wait(timeout=5)
        result["exit_code"] = self._proc.poll()
        if self._reader is not None:
            self._reader.join(timeout=5)
        try:
            if self._proc.stdout and not self._proc.stdout.closed:
                self._proc.stdout.close()
        except OSError:
            pass
        if self._stderr_handle is not None:
            try:
                self._stderr_handle.close()
            except OSError:
                pass
            self._stderr_handle = None
        self.log.info("driver closed", **{"exit_code": result["exit_code"]})
        return result


def test_owned_env(state_root: str) -> dict:
    """Build a child environment that redirects durable state into ``state_root``.

    Linux: XDG_STATE_HOME; Windows: LOCALAPPDATA; macOS: HOME (state falls
    back to $HOME/Library/Application Support). XDG_* and TMP are also
    redirected defensively. Existing secrets-bearing variables are passed
    through untouched otherwise.
    """
    env = dict(os.environ)
    env.pop("LOCAL_MCP_MANAGED_WORKTREE_ROOT", None)
    state_root = os.path.abspath(state_root)
    if sys.platform == "win32":
        # Empirically verified: the product resolves its state dir from the
        # profile (USERPROFILE) — <USERPROFILE>/AppData/Local/local-mcp —
        # not from the LOCALAPPDATA variable. Redirect the profile root.
        env["USERPROFILE"] = state_root
        env["HOME"] = state_root
        env["LOCALAPPDATA"] = os.path.join(state_root, "AppData", "Local")
    elif sys.platform == "darwin":
        env["HOME"] = state_root
    else:
        env["XDG_STATE_HOME"] = os.path.join(state_root, "state")
    env.setdefault("XDG_CONFIG_HOME", os.path.join(state_root, "config"))
    env.setdefault("XDG_DATA_HOME", os.path.join(state_root, "data"))
    env["XDG_CACHE_HOME"] = os.path.join(state_root, "cache")
    return env


def state_dir_base(state_root: str) -> str:
    """Per-platform base the server resolves as ``config::state_dir()`` root.

    Must stay aligned with ``test_owned_env``: the child resolves its state
    dir from the redirected variables, so the harness writes session files
    under the same root the server will read.
    """
    state_root = os.path.abspath(state_root)
    if sys.platform == "win32":
        # USERPROFILE is redirected to state_root; the server's
        # data_local_dir = <profile>/AppData/Local.
        return os.path.join(state_root, "AppData", "Local")
    if sys.platform == "darwin":
        return os.path.join(state_root, "Library", "Application Support")  # HOME
    return os.path.join(state_root, "state")  # XDG_STATE_HOME


def new_session_id(seed: int) -> str:
    """Deterministic, test-owned session id (reproducible runs, never operator)."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"soak-harness-v1-seed-{seed}"))
