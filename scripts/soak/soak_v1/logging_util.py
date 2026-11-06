"""Structured JSONL logging (Soak Harness V1, Slice 1)."""

from __future__ import annotations

import json
import sys
import threading
import time


class JsonLogger:
    LEVELS = {"debug": 10, "info": 20, "warn": 30, "error": 40}

    def __init__(self, path: str | None, level: str = "info", stream=None) -> None:
        self._handle = open(path, "a", encoding="utf-8") if path else None
        self._stream = stream or sys.stderr
        self._level = self.LEVELS.get(level, 20)
        self._lock = threading.Lock()

    def log(self, level: str, event: str, **fields) -> None:
        if self.LEVELS.get(level, 20) < self._level:
            return
        record = {"t": round(time.time(), 3), "level": level, "event": event, **fields}
        line = json.dumps(record, separators=(",", ":"), default=str)
        with self._lock:
            if self._handle:
                try:
                    self._handle.write(line + "\n")
                    self._handle.flush()
                except OSError:
                    pass
            try:
                self._stream.write(line + "\n")
                self._stream.flush()
            except OSError:
                pass

    def debug(self, event: str, **fields) -> None:
        self.log("debug", event, **fields)

    def info(self, event: str, **fields) -> None:
        self.log("info", event, **fields)

    def warn(self, event: str, **fields) -> None:
        self.log("warn", event, **fields)

    def error(self, event: str, **fields) -> None:
        self.log("error", event, **fields)

    def close(self) -> None:
        if self._handle:
            try:
                self._handle.close()
            except OSError:
                pass
            self._handle = None
