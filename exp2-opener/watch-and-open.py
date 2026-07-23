#!/usr/bin/env python3
"""Experiment 2: window-opener test.

Waits for the next 5h window reset, verifies the boundary is quiet, fires a
minimal opener prompt, then confirms a fresh window via usage_history.
Writes findings to exp2-opener/RESULT.md. Safe to re-run; aborts cleanly on
a spoiled boundary (human prompt got there first).
"""
import json
import sqlite3
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

DB = Path.home() / ".omp/agent/agent.db"
OUT = Path(__file__).parent / "RESULT.md"
LOG = Path(__file__).parent / "watch.log"
WORKDIR = Path("/tmp/exp2-opener-session")


def log(msg: str) -> None:
    line = f"{datetime.now().strftime('%H:%M:%S')} {msg}"
    print(line, flush=True)
    with open(LOG, "a") as f:
        f.write(line + "\n")


def latest_5h() -> dict | None:
    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    row = con.execute(
        "SELECT recorded_at, used_fraction, status, resets_at FROM usage_history "
        "WHERE limit_id = 'anthropic:5h' ORDER BY id DESC LIMIT 1"
    ).fetchone()
    con.close()
    return dict(row) if row else None


def force_probe() -> None:
    """Cheapest way to refresh usage_history without inference: omp usage probe.

    `omp -p` with a trivial prompt would open a window — NOT what we want for
    probing. Instead rely on the fact that any omp startup refreshes usage.
    We read passively first; only after the opener do we compare.
    """


def fire_opener() -> tuple[float, str]:
    WORKDIR.mkdir(exist_ok=True)
    t0 = time.time()
    r = subprocess.run(
        ["omp", "-p", "Reply with exactly: ok"],
        cwd=WORKDIR, capture_output=True, text=True, timeout=600,
    )
    return time.time() - t0, (r.stdout or r.stderr)[-200:]


def opener_cost() -> dict:
    sess = sorted(
        (Path.home() / ".omp/agent/sessions").glob("-tmp-exp2-opener-session/*.jsonl"),
        key=lambda f: f.stat().st_mtime,
    )
    agg = {"calls": 0, "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
    if not sess:
        return agg
    with open(sess[-1]) as fh:
        for line in fh:
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            msg = rec.get("message") or {}
            u = msg.get("usage")
            if u and msg.get("role") == "assistant":
                agg["calls"] += 1
                for k in ("input", "output", "cacheRead", "cacheWrite"):
                    agg[k] += u.get(k, 0) or 0
    return agg


def main() -> int:
    state = latest_5h()
    log(f"start: latest 5h row = {state}")
    if not state:
        log("ABORT: no 5h usage rows")
        return 1

    resets_at = state["resets_at"]
    now_ms = time.time() * 1000

    if resets_at and resets_at > now_ms:
        wait_s = (resets_at - now_ms) / 1000 + 120  # boundary + 2 min settle
        log(f"reset at {datetime.fromtimestamp(resets_at/1000).strftime('%H:%M:%S')}, sleeping {wait_s:.0f}s")
        time.sleep(wait_s)
    else:
        log("no future resets_at — window may already be past reset; proceeding")

    pre = latest_5h()
    log(f"pre-opener state: {pre}")
    # Spoiled? A fresh row recorded after the reset with used_fraction > 0.05
    if pre and resets_at and pre["recorded_at"] > resets_at and (pre["used_fraction"] or 0) > 0.05:
        report(f"SPOILED: window already opened by other traffic ({pre}). Re-arm at next reset.", pre, None, None)
        return 2

    log("boundary quiet — firing opener")
    dur, tail = fire_opener()
    cost = opener_cost()
    time.sleep(90)  # allow a usage refresh cycle
    post = latest_5h()
    log(f"post-opener state: {post}")
    report("COMPLETED", pre, {"duration_s": round(dur, 1), "output_tail": tail.strip(), **cost}, post)
    return 0


def report(status: str, pre, opener, post) -> None:
    OUT.write_text(f"""# Experiment 2 result — {datetime.now().isoformat(timespec='seconds')}

status: {status}

pre-opener 5h state: {pre}
opener: {opener}
post-opener 5h state: {post}

Interpretation guide: success = post shows a fresh window (low used_fraction,
new resets_at ≈ opener time + 5h). Opener cost fields show what the cheapest
`omp -p` opener actually costs (session floor applies — compare against a
future direct-API opener).
""")
    log(f"result written: {status}")


if __name__ == "__main__":
    sys.exit(main())
