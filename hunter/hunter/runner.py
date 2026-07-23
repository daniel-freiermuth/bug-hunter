"""Worker runner — spawns headless omp, meters its ledger, kills at cap.

The cap design (exp1b): never trust harness cooperation. The worker's session
JSONL is written live under ~/.omp/agent/sessions/; we watch it and SIGTERM
the process group at the token threshold. The ledger survives kills cleanly
(observed: zero corrupt lines after SIGTERM; partial trailing line possible
mid-write — skipped).
"""
from __future__ import annotations

import json
import os
import signal
import subprocess
import tempfile
import time
from pathlib import Path

from .types import OMP_SESSIONS_DIR, Config, RunResult


def ledger_usage(session_file: Path, since_iso: str = "") -> tuple[int, int]:
    """Sum 'new' tokens (input+output+cacheWrite) and call count.

    `since_iso` filters to records newer than the given UTC ISO timestamp —
    required when omp REUSES a prior session file for the same cwd (observed:
    a second `omp -p` run appends to the existing JSONL). The ledger's
    fixed-format timestamps ("2026-07-23T15:38:23.224Z") compare
    lexicographically.
    """
    tokens = calls = 0
    try:
        with open(session_file) as fh:
            for line in fh:
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue  # partial trailing line mid-write
                if since_iso and (rec.get("timestamp") or "") < since_iso:
                    continue
                msg = rec.get("message") or {}
                u = msg.get("usage")
                if u and msg.get("role") == "assistant":
                    calls += 1
                    tokens += (u.get("input") or 0) + (u.get("output") or 0) + (u.get("cacheWrite") or 0)
    except OSError:
        pass
    return tokens, calls


def _snapshot() -> dict[Path, int]:
    try:
        return {p: p.stat().st_size for p in OMP_SESSIONS_DIR.glob("*/*.jsonl")}
    except OSError:
        return {}


def _discover(before: dict[Path, int], cwd: Path) -> Path | None:
    """Find the worker's ledger: a file that appeared OR GREW since spawn.

    omp may create a fresh session file or append to an existing one for the
    same cwd. The session-dir slug algorithm is not guaranteed; prefer a
    candidate whose parent dir name fuzzily matches the cwd, else the most
    recently modified candidate.
    """
    candidates = []
    for p, size in _snapshot().items():
        old = before.get(p)
        if old is None or size > old:
            candidates.append(p)
    if not candidates:
        return None
    slug = str(cwd).replace("/", "-").strip("-")
    matches = [p for p in candidates
               if slug in p.parent.name.strip("-") or p.parent.name.strip("-") in slug]
    pool = matches or candidates
    return max(pool, key=lambda p: p.stat().st_mtime)


def _kill_tree(proc: subprocess.Popen) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait(timeout=10)


def run_worker(cfg: Config, cwd: Path, prompt: str, cap_tokens: int, max_wall_s: int,
               model: str | None = None) -> RunResult:
    before = _snapshot()
    t0 = time.time()
    spawn_iso = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(t0)) + ".000Z"
    cmd = [cfg.omp_bin, "-p", prompt]
    if model:
        cmd += [f"--model={model}"]
    if cfg.model_smol:
        cmd += [f"--smol={cfg.model_smol}"]
    out = tempfile.TemporaryFile(mode="w+")
    proc = subprocess.Popen(
        cmd,
        cwd=cwd, stdout=out, stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    session: Path | None = None
    tokens = calls = 0
    killed: str | None = None

    while True:
        rc = proc.poll()
        if session is None:
            session = _discover(before, cwd)
        if session is not None:
            tokens, calls = ledger_usage(session, spawn_iso)
        if rc is not None:
            break
        if tokens >= cap_tokens:
            killed = "cap"
            _kill_tree(proc)
            break
        if time.time() - t0 > max_wall_s:
            killed = "wallclock"
            _kill_tree(proc)
            break
        time.sleep(cfg.poll_s)

    exit_code = proc.wait()
    if session is not None:
        tokens, calls = ledger_usage(session, spawn_iso)
    out.seek(0)
    tail = out.read()[-2000:]
    out.close()
    return RunResult(
        exit_code=exit_code,
        killed_reason=killed,
        tokens_new=tokens,
        calls=calls,
        session_file=str(session) if session else None,
        duration_s=round(time.time() - t0, 1),
        stdout_tail=tail,
    )
