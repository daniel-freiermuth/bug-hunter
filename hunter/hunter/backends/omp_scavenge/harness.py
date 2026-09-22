"""Worker runner -- spawns headless omp, meters its ledger, kills at cap.

The cap design (exp1b): never trust harness cooperation. The worker's session
JSONL is written live into a private per-run directory; we watch it and SIGTERM
the process group at the token threshold. The ledger survives kills cleanly
(observed: zero corrupt lines after SIGTERM; partial trailing line possible
mid-write -- skipped).

Every run gets its own ``--session-dir``. omp's ``autoResume`` setting makes a
bare ``omp -p`` continue the newest session for the same cwd whenever no
session flag or session directory is passed, and a resumed worker re-caches the
entire prior transcript on its first call (observed: 508 709 cacheWrite tokens
on call #1 of a repo whose session had been growing since 2026-09-06), which
trips any cap before the worker does anything useful.
"""

from __future__ import annotations

import contextlib
import itertools
import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from typing import TYPE_CHECKING

from hunter.types import Config, RunResult

if TYPE_CHECKING:
    from pathlib import Path

# omp-specific path; lives here rather than in core types.
# How many worker transcripts to keep under ``<work_root>/sessions``.
# One directory per run, never reused, so without a bound this grows for
# the life of the deployment. Keeping the most recent N leaves enough to
# debug a failure noticed days later, which is what these are read for
# once metering is done with them. Mirrors hunter-rs.
SESSIONS_RETAINED = 50
_RUN_SEQ = itertools.count()


def ledger_usage(session_file: Path) -> tuple[int, int]:
    """Sum 'new' tokens (input+output+cacheWrite) and call count."""
    tokens = calls = 0
    try:
        with session_file.open() as fh:
            for line in fh:
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                msg = rec.get("message") or {}
                u = msg.get("usage")
                if u and msg.get("role") == "assistant":
                    calls += 1
                    tokens += (
                        (u.get("input") or 0) + (u.get("output") or 0) + (u.get("cacheWrite") or 0)
                    )
    except OSError:
        pass
    return tokens, calls


def sessions_root(work_root: Path) -> Path:
    """Root for per-run session directories: ``<work_root>/sessions``.

    Hunter's own data under hunter's own work root, NOT the operator's
    ``~/.omp/agent/sessions``: a directory per run in that shared tree grows
    without bound inside a directory a human also uses, and nothing else
    prunes it. Being easy to find is served just as well by a documented
    path we control.
    """
    return work_root / "sessions"


def _run_session_dir(work_root: Path, cwd: Path, spawn_ms: int) -> Path:
    """Private session directory for one worker run.

    Named with omp's own cwd slug plus the spawn instant and a process-local
    counter, so two runs in the same worktree -- retries of one job -- never
    share a directory and so never resume one another.
    """
    slug = str(cwd).replace("/", "-").strip("-")
    return sessions_root(work_root) / f"{slug}--{spawn_ms}-{next(_RUN_SEQ)}"


def prune_sessions(work_root: Path) -> int:
    """Drop all but the newest ``SESSIONS_RETAINED`` run directories.

    Called before a run creates its own, so the live directory is never a
    candidate. Best effort throughout: a transcript that cannot be removed
    is a disk-space problem, not a reason to refuse the job about to start.
    """
    root = sessions_root(work_root)
    try:
        dirs = [p for p in root.iterdir() if p.is_dir()]
    except OSError:
        return 0
    if len(dirs) <= SESSIONS_RETAINED:
        return 0
    dirs.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    removed = 0
    for path in dirs[SESSIONS_RETAINED:]:
        try:
            shutil.rmtree(path)
        except OSError:
            continue
        removed += 1
    return removed


def _ledger_dir_usage(run_dir: Path) -> tuple[Path, int, int] | None:
    """Meter every ``*.jsonl`` omp wrote for this run; None until one exists.

    One file is the norm; summing rather than picking one means an extra
    transcript (a nested session) counts as spend instead of being discounted.
    """
    try:
        files = sorted(p for p in run_dir.glob("*.jsonl"))
    except OSError:
        return None
    if not files:
        return None
    tokens = calls = 0
    for f in files:
        t, c = ledger_usage(f)
        tokens += t
        calls += c
    # omp names sessions by creation instant, so the first is the run's own.
    return files[0], tokens, calls


def _kill_tree(proc: subprocess.Popen[str] | subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=10)


def run_worker(
    cfg: Config,
    cwd: Path,
    prompt: str,
    cap_tokens: int,
    max_wall_s: int,
    model: str | None = None,
) -> RunResult:
    t0 = time.time()
    prune_sessions(cfg.work_root)
    run_dir = _run_session_dir(cfg.work_root, cwd, int(t0 * 1000))
    run_dir.mkdir(parents=True, exist_ok=True)
    cmd = [cfg.omp_bin, "-p", prompt, f"--session-dir={run_dir}"]
    if model:
        cmd += [f"--model={model}"]
    if cfg.model_smol:
        cmd += [f"--smol={cfg.model_smol}"]
    out = tempfile.TemporaryFile(mode="w+")
    # Build a minimal env so the worker doesn't inherit the full daemon env.
    allowlist = {
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SSH_AUTH_SOCK",
    }
    prefixes = ("OMP_", "XDG_")
    worker_env = {k: v for k, v in os.environ.items() if k in allowlist or k.startswith(prefixes)}

    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        stdout=out,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        env=worker_env,
    )
    session: Path | None = None
    tokens = calls = 0
    killed: str | None = None

    while True:
        rc = proc.poll()
        metered = _ledger_dir_usage(run_dir)
        if metered is not None:
            session, tokens, calls = metered
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
    metered = _ledger_dir_usage(run_dir)
    if metered is not None:
        session, tokens, calls = metered
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
