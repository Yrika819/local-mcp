#!/usr/bin/env python3
import argparse, json, os, subprocess, time, uuid, pathlib, statistics, threading

def _child_working_set_kb(pid):
    try:
        import ctypes
        from ctypes import wintypes
        class PMC(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t),
            ]
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        h = ctypes.windll.kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not h:
            return None
        pmc = PMC(); pmc.cb = ctypes.sizeof(PMC)
        ok = ctypes.windll.psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb)
        ctypes.windll.kernel32.CloseHandle(h)
        if not ok:
            return None
        return pmc.WorkingSetSize // 1024
    except Exception:
        return None

def pct(sorted_lats, p):
    if not sorted_lats:
        return None
    idx = int(p * len(sorted_lats))
    if idx >= len(sorted_lats):
        idx = len(sorted_lats) - 1
    return sorted_lats[idx]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--duration", type=int, default=300)
    ap.add_argument("--concurrency", type=int, default=4)
    ap.add_argument("--state-dir", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    state = pathlib.Path(args.state_dir)
    if state.exists():
        import shutil; shutil.rmtree(state)
    state.mkdir(parents=True)
    sessions_dir = state / "local-mcp" / "sessions"
    sessions_dir.mkdir(parents=True)
    cwd = state / "cwd"; cwd.mkdir()
    sid = str(uuid.uuid4())
    (sessions_dir / f"{sid}.json").write_text(json.dumps(
        {"id": sid, "cwd": str(cwd), "permitted_directories": [str(cwd)]}))

    env = os.environ.copy()
    env["LOCALAPPDATA"] = str(state)
    env["APPDATA"] = str(state)

    proc = subprocess.Popen([args.binary, "mcp"], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, env=env, bufsize=1)
    lock = threading.Lock()
    pending = {}
    lats = []
    error_count = 0
    bytes_read = 0
    stop = threading.Event()
    stderr_buf = []
    child_gone = threading.Event()

    def reader():
        nonlocal error_count, bytes_read
        while not stop.is_set():
            line = proc.stdout.readline()
            if not line:
                break
            bytes_read += len(line)
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                continue
            rid = obj.get("id")
            t = time.time()
            err = "error" in obj
            with lock:
                if rid in pending:
                    lat = t - pending.pop(rid)
                    lats.append(lat)
                    if err:
                        error_count += 1
                del obj
    rt = threading.Thread(target=reader, daemon=True)
    rt.start()

    mem_samples = []
    start_ref = [time.time()]
    def mem_monitor():
        while not stop.is_set():
            kb = _child_working_set_kb(proc.pid)
            if kb is not None:
                mem_samples.append((round(time.time() - start_ref[0], 1), kb))
            stop.wait(60)
    mt = threading.Thread(target=mem_monitor, daemon=True)
    mt.start()

    def err_reader():
        while not stop.is_set():
            line = proc.stderr.readline()
            if not line:
                break
            stderr_buf.append(line.rstrip())
    et = threading.Thread(target=err_reader, daemon=True)
    et.start()

    req_id = 0
    rid_lock = threading.Lock()
    inflight_at_exit = {}

    def send_one(method):
        nonlocal req_id
        with rid_lock:
            req_id += 1; rid = req_id
        t0 = time.time()
        req = {"jsonrpc": "2.0", "id": rid, "method": method}
        with lock:
            pending[rid] = t0
        try:
            proc.stdin.write(json.dumps(req) + "\n"); proc.stdin.flush()
        except (OSError, BrokenPipeError, ValueError):
            stop.set()
            raise
        return rid

    methods = ["ping", "tools/list"]
    start = time.time()
    end = start + args.duration
    import random
    early = None
    try:
        while time.time() < end and not stop.is_set():
            if proc.poll() is not None:
                child_gone.set()
                break
            for _ in range(args.concurrency):
                if stop.is_set():
                    break
                send_one(random.choice(methods))
            time.sleep(0.05)
    except Exception as e:
        early = repr(e)

    elapsed = time.time() - start

    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.wait(timeout=10)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass

    time.sleep(0.5)
    stop.set()
    with lock:
        inflight_at_exit = dict(pending)

    sorted_lats = sorted(v * 1000 for v in lats)
    summary = {
        "requested_duration_sec": args.duration,
        "elapsed_sec": round(elapsed, 1),
        "completed_full_duration": elapsed >= args.duration - 2 and early is None and not child_gone.is_set(),
        "requests": len(sorted_lats),
        "success": len(sorted_lats) - error_count,
        "error_responses": error_count,
        "inflight_at_exit": len(inflight_at_exit),
        "response_bytes_read": bytes_read,
        "child_returncode": proc.returncode,
        "child_exited_early": child_gone.is_set() or (proc.returncode is not None and elapsed < args.duration - 2),
        "early_error": early,
        "stderr_tail": stderr_buf[-40:],
        "child_working_set_kb_start": mem_samples[0][1] if mem_samples else None,
        "child_working_set_kb_end": mem_samples[-1][1] if mem_samples else None,
        "child_working_set_kb_max": max((m[1] for m in mem_samples), default=None),
        "mem_samples": mem_samples[::5],
        "p50_ms": statistics.median(sorted_lats) if sorted_lats else None,
        "p95_ms": pct(sorted_lats, 0.95),
        "p99_ms": pct(sorted_lats, 0.99),
        "max_ms": max(sorted_lats) if sorted_lats else None,
    }
    pathlib.Path(args.out).write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2))

if __name__ == "__main__":
    main()
