"""Resource and latency measurement (Soak Harness V1, Slice 4).

Bounds come from the design defaults (docs/SOAK_HARNESS_V1_DESIGN.md §2/§5)
and are always explicit CLI-visible values. A bound that cannot be evaluated
at runtime makes the run NOT RUN, never PASS. Working-set sampling uses
/proc/<pid>/statm on Linux and GetProcessMemoryInfo (ctypes) on Windows;
on macOS best-effort ``ps`` sampling is used and flagged approximate.
"""

from __future__ import annotations

import os
import random
import subprocess
import sys
import threading
import time

# Design defaults (§2). Never implicit: every bound is also a CLI flag.
DEFAULT_MAX_INFLIGHT = 128
DEFAULT_WIRE_LINE_CAP = 16 * 1024 * 1024
DEFAULT_RESPONSE_BYTE_CAP = 1024 * 1024 * 1024
DEFAULT_WORKING_SET_CAP = int(1.5 * 1024 * 1024 * 1024)
DEFAULT_WORKING_SET_GROWTH_FRACTION = 0.50
# Slice-3 adjustment (recorded, never removed): the design's "+50 % of
# baseline" alone produces hair-trigger bounds when the baseline working set
# is tiny (e.g. ~13 MB on Windows). The absolute cap (1.5 GiB) is unchanged;
# the growth allowance gains an absolute floor so the bound stays meaningful.
DEFAULT_WORKING_SET_GROWTH_FLOOR_BYTES = 256 * 1024 * 1024
DEFAULT_FD_CAP = 4096
DEFAULT_MAX_LATENCY_SAMPLES = 10_000
DEFAULT_SAMPLE_INTERVAL_S = 5.0

MEASURED = "MEASURED"
NOT_RUN = "NOT RUN"


class ResourceBounds:
    def __init__(
        self,
        max_inflight: int = DEFAULT_MAX_INFLIGHT,
        wire_line_cap: int = DEFAULT_WIRE_LINE_CAP,
        response_byte_cap: int = DEFAULT_RESPONSE_BYTE_CAP,
        working_set_cap: int = DEFAULT_WORKING_SET_CAP,
        working_set_growth_fraction: float = DEFAULT_WORKING_SET_GROWTH_FRACTION,
        working_set_growth_floor_bytes: int = DEFAULT_WORKING_SET_GROWTH_FLOOR_BYTES,
        fd_cap: int = DEFAULT_FD_CAP,
        max_latency_samples: int = DEFAULT_MAX_LATENCY_SAMPLES,
    ) -> None:
        self.max_inflight = max_inflight
        self.wire_line_cap = wire_line_cap
        self.response_byte_cap = response_byte_cap
        self.working_set_cap = working_set_cap
        self.working_set_growth_fraction = working_set_growth_fraction
        self.working_set_growth_floor_bytes = working_set_growth_floor_bytes
        self.fd_cap = fd_cap
        self.max_latency_samples = max_latency_samples

    def as_dict(self) -> dict:
        return {
            "max_inflight": self.max_inflight,
            "wire_line_cap_bytes": self.wire_line_cap,
            "response_byte_cap_bytes": self.response_byte_cap,
            "working_set_cap_bytes": self.working_set_cap,
            "working_set_growth_fraction": self.working_set_growth_fraction,
            "working_set_growth_floor_bytes": self.working_set_growth_floor_bytes,
            "fd_cap": self.fd_cap,
            "max_latency_samples": self.max_latency_samples,
        }


def _working_set_linux(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/statm", "rb") as handle:
            fields = handle.read().split()
        pages = int(fields[1])
        return pages * os.sysconf("SC_PAGE_SIZE")
    except (OSError, IndexError, ValueError):
        return None


def _fd_count_linux(pid: int) -> int | None:
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return None


def _working_set_windows(pid: int) -> int | None:
    import ctypes
    from ctypes import wintypes

    class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
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

    psapi = ctypes.WinDLL("psapi.dll", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32.dll", use_last_error=True)
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        return None
    try:
        counters = PROCESS_MEMORY_COUNTERS()
        counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
        if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            return None
        return int(counters.WorkingSetSize)
    finally:
        kernel32.CloseHandle(handle)


def _handle_count_windows(pid: int) -> int | None:
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32.dll", use_last_error=True)
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        return None
    try:
        count = wintypes.DWORD(0)
        if not kernel32.GetProcessHandleCount(handle, ctypes.byref(count)):
            return None
        return int(count.value)
    finally:
        kernel32.CloseHandle(handle)


def _working_set_macos(pid: int) -> int | None:
    """Best-effort RSS via ``ps`` (approximate; recorded as such)."""
    try:
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout.strip()
        return int(out) * 1024 if out else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def _fd_count_macos(pid: int) -> int | None:
    try:
        out = subprocess.run(
            ["bash", "-c", f"lsof -p {pid} 2>/dev/null | wc -l"],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout.strip()
        return int(out) if out else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def _working_set_fallback(pid: int) -> int | None:
    """Unrecognized platform: no sampler is available."""
    return None


def working_set_sampler(platform: str = sys.platform):
    if platform.startswith("linux"):
        return _working_set_linux
    if platform == "win32":
        return _working_set_windows
    if platform == "darwin":
        return _working_set_macos
    return _working_set_fallback


def fd_count_sampler(platform: str = sys.platform):
    if platform.startswith("linux"):
        return _fd_count_linux
    if platform == "win32":
        return _handle_count_windows
    if platform == "darwin":
        return _fd_count_macos
    return _working_set_fallback


def platform_supports_resource_sampling(platform: str = sys.platform) -> bool:
    return platform.startswith("linux") or platform in ("win32", "darwin")


class LatencyReservoir:
    """Bounded latency-sample store: reservoir sampling past the cap."""

    def __init__(self, cap: int = DEFAULT_MAX_LATENCY_SAMPLES, seed: int = 0) -> None:
        self.cap = cap
        self._rng = random.Random(seed)
        self._samples: list[float] = []
        self._seen = 0
        self._dropped = 0

    def add(self, latency_s: float) -> None:
        self._seen += 1
        if len(self._samples) < self.cap:
            self._samples.append(latency_s)
            return
        index = self._rng.randint(0, self._seen - 1)
        if index < self.cap:
            self._samples[index] = latency_s
        else:
            self._dropped += 1

    def stats(self) -> dict:
        if len(self._samples) < 100:
            return {
                "p50_ms": "NOT-MEASURED",
                "p95_ms": "NOT-MEASURED",
                "p99_ms": "NOT-MEASURED",
                "samples": len(self._samples),
                "seen": self._seen,
                "dropped": self._dropped,
            }
        ordered = sorted(self._samples)
        return {
            "p50_ms": round(_percentile(ordered, 50) * 1000, 3),
            "p95_ms": round(_percentile(ordered, 95) * 1000, 3),
            "p99_ms": round(_percentile(ordered, 99) * 1000, 3),
            "samples": len(self._samples),
            "seen": self._seen,
            "dropped": self._dropped,
        }


def _percentile(ordered: list, pct: float) -> float:
    """Linear-interpolated percentile over an ascending list."""
    if not ordered:
        raise ValueError("no samples")
    if len(ordered) == 1:
        return ordered[0]
    rank = (pct / 100.0) * (len(ordered) - 1)
    lower = int(rank)
    upper = min(lower + 1, len(ordered) - 1)
    weight = rank - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


class ResourceSampler(threading.Thread):
    """Samples the child every ``interval_s`` seconds; records MEASURED/NOT RUN."""

    def __init__(self, pid: int, bounds: ResourceBounds, interval_s: float = DEFAULT_SAMPLE_INTERVAL_S) -> None:
        super().__init__(name="soak-sampler", daemon=True)
        self.pid = pid
        self.bounds = bounds
        self.interval_s = interval_s
        self._stop_event = threading.Event()
        self.samples: list = []
        self.ws_supported = platform_supports_resource_sampling()
        self._ws = working_set_sampler()
        self._fds = fd_count_sampler()
        self.violations: list = []
        self.peak_working_set = 0
        self.peak_fds = 0
        self.status = NOT_RUN if not self.ws_supported else MEASURED
        self.status_detail = "" if self.ws_supported else (
            "no resource sampler for this platform; resource bounds NOT RUN"
        )

    def run(self) -> None:
        while not self._stop_event.wait(self.interval_s):
            self.sample_once()

    def sample_once(self) -> dict:
        sample = {
            "t": round(time.time(), 3),
            "working_set_bytes": None,
            "fds": None,
        }
        if self._ws is not None and self.ws_supported:
            sample["working_set_bytes"] = self._ws(self.pid)
            sample["fds"] = self._fds(self.pid)
        self.samples.append(sample)
        ws = sample["working_set_bytes"]
        if ws is not None:
            self.peak_working_set = max(self.peak_working_set, ws)
        fds = sample["fds"]
        if fds is not None:
            self.peak_fds = max(self.peak_fds, fds)
        return sample

    def baseline_working_set(self) -> int | None:
        for sample in self.samples:
            if sample["working_set_bytes"] is not None:
                return sample["working_set_bytes"]
        return None

    def working_set_bound(self) -> int | None:
        baseline = self.baseline_working_set()
        if baseline is None:
            return None
        growth = max(
            int(baseline * self.bounds.working_set_growth_fraction),
            self.bounds.working_set_growth_floor_bytes,
        )
        return min(self.bounds.working_set_cap, baseline + growth)

    def check_bounds(self, max_inflight_observed: int, response_bytes: int) -> list:
        violations = []
        if not self.ws_supported or self.status != MEASURED:
            return violations
        bound = self.working_set_bound()
        if bound is not None and self.peak_working_set > bound:
            violations.append(
                f"working set {self.peak_working_set} > bound {bound}"
            )
        if self.peak_fds and self.peak_fds > self.bounds.fd_cap:
            violations.append(f"fd count {self.peak_fds} > cap {self.bounds.fd_cap}")
        if max_inflight_observed > self.bounds.max_inflight:
            violations.append(
                f"in-flight {max_inflight_observed} > bound {self.bounds.max_inflight}"
            )
        if response_bytes > self.bounds.response_byte_cap:
            violations.append(
                f"response bytes {response_bytes} > cap {self.bounds.response_byte_cap}"
            )
        self.violations = violations
        return violations

    def stop(self) -> None:
        self._stop_event.set()
        self.join(timeout=self.interval_s + 5)

    def summary(self) -> dict:
        return {
            "status": self.status,
            "detail": self.status_detail,
            "peak_working_set_bytes": self.peak_working_set if self.status == MEASURED else None,
            "peak_fd_count": self.peak_fds if self.status == MEASURED else None,
            "working_set_bound_bytes": self.working_set_bound() if self.status == MEASURED else None,
            "samples": len(self.samples),
        }
