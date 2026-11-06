"""Test-owned stub MCP stdio server for Soak Harness V1 unit tests.

Speaks newline-delimited JSON-RPC like the product and exposes deliberate
behaviors used by tests. Never touches durable state.
"""

from __future__ import annotations

import json
import os
import signal
import sys
import time

REQUEST_FRAME_CAP = int(os.environ.get("STUB_FRAME_CAP", str(8 * 1024 * 1024)))


def read_line_bounded(stream) -> bytes:
    """Line reader with a configurable frame cap (fast, test-owned)."""
    line = stream.readline()
    if not line:
        return b""
    if len(line) > REQUEST_FRAME_CAP:
        return b"__TOO_LARGE__"
    return line.rstrip(b"\n") or (b"" if line == b"\n" else line)


def main() -> int:
    out = sys.stdout.buffer
    behavior = os.environ.get("STUB_BEHAVIOR", "normal")
    while True:
        line = read_line_bounded(sys.stdin.buffer)
        if not line:
            break
        if line == b"__TOO_LARGE__":
            out.write(json.dumps(
                {"jsonrpc": "2.0", "id": None,
                 "error": {"code": -32001, "message": "request frame exceeded the maximum"}}).encode())
            out.write(b"\n")
            out.flush()
            continue
        try:
            frame = json.loads(line.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            out.write(json.dumps(
                {"jsonrpc": "2.0", "id": None,
                 "error": {"code": -32700, "message": "parse error"}}).encode())
            out.write(b"\n")
            out.flush()
            continue
        method = frame.get("method", "")
        request_id = frame.get("id")
        if request_id is None:
            continue  # notification: no response by design
        name = str(method)
        if behavior == "no-response":
            continue
        if name == "initialize":
            result = {"protocolVersion": "2025-06-18", "serverInfo": {"name": "stub-mcp"}}
        elif name == "ping":
            result = {}
        elif name == "tools/call":
            tool = frame.get("params", {}).get("name", "")
            result = {"content": [{"type": "text", "text": f"stub:{tool}"}]}
        elif name == "test.error":
            out.write(json.dumps(
                {"jsonrpc": "2.0", "id": request_id,
                 "error": {"code": -32603, "message": "stub error"}}).encode())
            out.write(b"\n")
            out.flush()
            continue
        elif name == "test.duplicate":
            payload = json.dumps({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}}).encode()
            out.write(payload + b"\n" + payload + b"\n")
            out.flush()
            continue
        elif name == "test.unknown":
            out.write(json.dumps(
                {"jsonrpc": "2.0", "id": 999999, "result": {}}).encode())
            out.write(b"\n")
            out.flush()
            continue
        elif name == "test.bad_frame":
            out.write(b"not-json-garbage\n")
            out.flush()
            continue
        else:
            result = {"echo": name}
        out.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}).encode())
        out.write(b"\n")
        out.flush()
    return 0


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    raise SystemExit(main())
