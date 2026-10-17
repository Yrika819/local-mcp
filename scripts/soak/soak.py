#!/usr/bin/env python3
import argparse, json, os, subprocess, time, uuid, pathlib, statistics, threading, signal

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
    (sessions_dir / f"{sid}.json").write_text(json.dumps({"id": sid, "cwd": str(cwd), "permitted_directories": [str(cwd)]}))

    env = os.environ.copy()
    env["LOCALAPPDATA"] = str(state)
    env["APPDATA"] = str(state)

    proc = subprocess.Popen([args.binary, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env, bufsize=1)
    # reader thread collects responses
    lock = threading.Lock()
    pending = {}
    results = []
    stop = threading.Event()

    def reader():
        while not stop.is_set():
            line = proc.stdout.readline()
            if not line:
                break
            line=line.strip()
            if not line: continue
            try:
                obj=json.loads(line)
            except Exception:
                continue
            rid=obj.get("id")
            t=time.time()
            with lock:
                if rid in pending:
                    lat=t-pending.pop(rid)
                    results.append((rid, lat, obj))
    rt=threading.Thread(target=reader, daemon=True)
    rt.start()

    req_id=0
    rid_lock=threading.Lock()
    def send_one(method):
        nonlocal req_id
        with rid_lock:
            req_id+=1; rid=req_id
        t0=time.time()
        req={"jsonrpc":"2.0","id":rid,"method":method}
        with lock:
            pending[rid]=t0
        proc.stdin.write(json.dumps(req)+"\n"); proc.stdin.flush()
        return rid

    methods=["ping","tools/list"]
    end=time.time()+args.duration
    import random
    while time.time()<end:
        for _ in range(args.concurrency):
            send_one(random.choice(methods))
        time.sleep(0.05)

    time.sleep(1)
    stop.set()
    proc.stdin.close()
    proc.wait(timeout=10)

    lats=[r[1]*1000 for r in results]
    summary={
        "duration_sec": args.duration,
        "requests": len(results),
        "success": len(results),
        "p50_ms": statistics.median(lats) if lats else None,
        "p95_ms": sorted(lats)[int(0.95*len(lats))-1] if lats else None,
        "p99_ms": sorted(lats)[int(0.99*len(lats))-1] if lats else None,
        "max_ms": max(lats) if lats else None,
    }
    pathlib.Path(args.out).write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2))

if __name__=="__main__":
    main()
