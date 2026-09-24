"""Worker runner -- spawns headless omp, meters its ledger, kills at cap.

The cap design (exp1b): never trust harness cooperation. The worker's session
JSONL is written live into a private per-run directory; we watch it and SIGTERM
the process group at the token threshold. The ledger survives kills cleanly
(observed: zero corrupt lines after SIGTERM; partial trailing line possible
mid-write -- skipped).

``cap_tokens`` may be None, which means the budget granted this job no token
bound at all and ``max_wall_s`` is the only thing that stops it.

Every run gets its own ``--session-dir``. omp's ``autoResume`` setting makes a
bare ``omp -p`` continue the newest session for the same cwd whenever no
session flag or session directory is passed, and a resumed worker re-caches the
entire prior transcript on its first call (observed: 508 709 cacheWrite tokens
on call #1 of a repo whose session had been growing since 2026-09-06), which
trips any cap before the worker does anything useful.

The exception is a deliberate continuation: ``run_worker`` can be handed the
session file of a job that ran out of window headroom mid-flight and continue
*that* transcript by naming its path.  That is a different mechanism from
``autoResume`` and the two must not be confused -- see the ``--resume``
argument in ``run_worker`` for what was measured.
"""

from __future__ import annotations

import contextlib
import itertools
import json
import logging
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

log = logging.getLogger("hunter.harness")

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


def ctx_at_suspension(session_file: Path) -> int | None:
    """Context the worker was carrying when the transcript stops, or None
    when the file cannot be read or holds no usage record.

    The LAST usage record's ``input + cacheRead + cacheWrite`` -- what the
    provider had to be shown to make that call, which is exactly what
    resuming will have to re-cache. Deliberately NOT ``ledger_usage``'s
    sum: that one adds ``output`` and skips ``cacheRead`` because it
    measures what a run SPENT, accumulated across calls. This measures
    what one call CARRIED, so it neither counts generated tokens (they are
    already in the transcript being resumed) nor drops the cached prefix
    (a resume pays for it again). Across 112 production re-cache events
    the re-cache / prior-context ratio had median 1.00 and p10 1.00, so
    this figure is the resume's cost, not a proxy for it.

    Same file format ``ledger_usage`` parses, and equally kill-tolerant: a
    truncated final line is skipped rather than raising, leaving the last
    intact record as the answer.
    """
    ctx: int | None = None
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
                    ctx = (
                        (u.get("input") or 0)
                        + (u.get("cacheRead") or 0)
                        + (u.get("cacheWrite") or 0)
                    )
    except OSError:
        return None
    return ctx


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


def _first_line(path: Path) -> bytes:
    """First line of a file, or empty on any IO error.

    Only ever compared against itself: omp writes a ``{"type":"title"...}``
    header as a session's opening record and never rewrites it, so that line
    is a cheap identity for "still the same session".
    """
    try:
        with path.open("rb") as fh:
            return fh.readline()
    except OSError:
        return b""


def _resume_unavailable(session_file: Path, why: str) -> RunResult:
    """A resume that cannot happen, reported rather than silently downgraded.

    The caller asked to continue one specific transcript.  If that transcript
    is gone, spawning a cold worker here would be indistinguishable from a
    successful resume anywhere downstream -- same exit code, same session
    path -- while paying a fresh session floor and redoing the work the resume
    existed to avoid.  Whether to start clean is the scheduler's decision, so
    the scheduler is the one told.
    """
    log.warning("harness: cannot resume %s: %s", session_file, why)
    return RunResult(
        exit_code=None,
        killed_reason="resume-unavailable",
        tokens_new=0,
        calls=0,
        session_file=None,
        duration_s=0.0,
        stdout_tail=f"cannot resume {session_file}: {why}",
    )


def _resume_target(session_file: Path) -> tuple[Path, bytes, int, int] | str:
    """Resolve a resume source, or say why it cannot be continued.

    Returns ``(run_dir, opening record, baseline tokens, baseline calls)``.

    The run directory is the predecessor's, not a new one: omp writes the
    continued session at the ``--resume`` path and ``--session-dir`` does not
    move it.  The baseline is the attribution rule -- a resumed worker appends
    to the predecessor's ledger and ``_ledger_dir_usage`` sums the whole
    directory, so every token already in it was paid for by the predecessor's
    job row.  Without subtracting it, each link of a resume chain re-bills its
    own history.
    """
    run_dir = session_file.parent
    if not run_dir.is_dir():
        return "session directory is gone"
    try:
        size = session_file.stat().st_size
    except OSError:
        size = 0
    # Empty counts as gone: omp treats an unreadable resume source as "start
    # fresh here", which is the failure this refuses.
    if size == 0:
        return "session file is missing or empty"
    base = _ledger_dir_usage(run_dir)
    base_tokens, base_calls = (base[1], base[2]) if base is not None else (0, 0)
    return run_dir, _first_line(session_file), base_tokens, base_calls


def _meter(
    run_dir: Path,
    base: tuple[int, int],
    prev: tuple[Path | None, int, int],
) -> tuple[Path | None, int, int]:
    """This attempt's share of the run directory's ledger, or ``prev``.

    The totals are clamped because the subtraction has one way to go negative:
    the transcript shrinking, i.e. being replaced rather than appended to.  A
    negative running total would sit below every cap and disarm the watchdog.
    """
    metered = _ledger_dir_usage(run_dir)
    if metered is None:
        return prev
    session, tokens, calls = metered
    return session, max(0, tokens - base[0]), max(0, calls - base[1])


def _worker_env() -> dict[str, str]:
    """A minimal environment for the worker, rather than the daemon's own.

    The daemon's environment carries provider credentials the worker has no
    business seeing, so it is rebuilt from an allowlist instead of inherited.
    """
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
    return {k: v for k, v in os.environ.items() if k in allowlist or k.startswith(prefixes)}


def _resume_lost(session_file: Path | None, head: bytes | None) -> bool:
    """Did a resume silently fail to take?

    omp answers in the one place it costs nothing to look: a fresh session
    written at the ``--resume`` path replaces the file's opening record, where
    a real continuation appends and leaves it untouched.  Nothing in the exit
    code distinguishes the two, and missing it means believing work was
    continued that was in fact redone from scratch at full price.
    """
    if session_file is None or head is None:
        return False
    if _first_line(session_file) == head:
        return False
    log.warning(
        "harness: --resume %s did not continue that session -- omp started a "
        "fresh one at the same path, so this attempt redid the work it was "
        "meant to continue",
        session_file,
    )
    return True


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
    cap_tokens: int | None,
    max_wall_s: int,
    model: str | None = None,
    resume_from: Path | None = None,
) -> RunResult:
    """Spawn ``omp -p``, meter its JSONL session ledger, kill at the cap.

    ``resume_from`` is the session file of an earlier attempt to continue.
    None starts cold in a private directory, which is what every job did
    before resume existed.  A path continues that transcript in place and
    meters only what this attempt adds to it; a source that is no longer on
    disk is refused outright (``killed_reason = "resume-unavailable"``)
    rather than quietly downgraded to a cold run.
    """
    t0 = time.time()
    resume_head: bytes | None = None
    base = (0, 0)
    if resume_from is None:
        # Pruning is only safe for a directory this run is about to create:
        # it keeps the newest N by mtime, and a transcript worth resuming is
        # old by construction.
        prune_sessions(cfg.work_root)
        run_dir = _run_session_dir(cfg.work_root, cwd, int(t0 * 1000))
        run_dir.mkdir(parents=True, exist_ok=True)
    else:
        target = _resume_target(resume_from)
        if isinstance(target, str):
            return _resume_unavailable(resume_from, target)
        run_dir, resume_head, base_tokens, base_calls = target
        base = (base_tokens, base_calls)
    cmd = [cfg.omp_bin, "-p", prompt, f"--session-dir={run_dir}"]
    if resume_from is not None:
        # Measured against omp v18.2.6 on 2026-09-24, because the obvious
        # reading of these two flags is that they conflict and they do not:
        # ``--resume=<path>`` and ``--session-dir=<dir>`` coexist, and under
        # ``-p`` the named session continues non-interactively.  Proof from
        # the probe: the resumed run's transcript kept the first exchange byte
        # for byte and chained its new records onto the old file's last one,
        # while the same prompt in a fresh session directory answered "there
        # is no earlier reply in this conversation".  This is emphatically not
        # omp's autoResume, which continues "the newest session for this cwd"
        # -- naming the path is what makes the continuation the one we meant.
        #
        # ``--resume`` also decides where the transcript is written: omp
        # appends to the named file and ``--session-dir`` does not override
        # that.  An unresolvable path is not an error -- omp starts a fresh
        # session, writes it at that path, and exits 0 with no diagnostic --
        # which is why the source is checked before this spawn instead of
        # trusted after it.
        #
        # Cost shape, same probe: the cold call wrote 22 976 cacheWrite and
        # read 0; the resumed call wrote 28 and read 22 976.  Inside the
        # prompt-cache TTL a resume re-caches essentially nothing.  Past it the
        # re-cache costs the context size at suspension (112 production
        # re-cache events, median ratio 1.00) -- still bounded by the
        # transcript, never by redoing the work that produced it.
        cmd += [f"--resume={resume_from}"]
    if model:
        cmd += [f"--model={model}"]
    if cfg.model_smol:
        cmd += [f"--smol={cfg.model_smol}"]
    out = tempfile.TemporaryFile(mode="w+")
    worker_env = _worker_env()

    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        # Explicitly closed, never inherited: omp reads a piped stdin as
        # extra prompt text and blocks on EOF before it initialises the
        # session, so the worker writes no ledger and is killed as
        # unmetered after the grace period, having spent its whole
        # wall-clock slot. The daemon only has /dev/null on fd 0 today
        # because the unit sets no StandardInput and systemd defaults to
        # null -- an inherited accident, not a decision.
        stdin=subprocess.DEVNULL,
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
        session, tokens, calls = _meter(run_dir, base, (session, tokens, calls))
        if rc is not None:
            break
        if cap_tokens is not None and tokens >= cap_tokens:
            killed = "cap"
            _kill_tree(proc)
            break
        if time.time() - t0 > max_wall_s:
            killed = "wallclock"
            _kill_tree(proc)
            break
        time.sleep(cfg.poll_s)

    exit_code = proc.wait()
    resume_lost = _resume_lost(resume_from, resume_head)
    if resume_lost:
        # A lost resume took the predecessor's transcript with it, so the
        # baseline measured against it no longer describes anything on disk:
        # whatever records remain were all written by this attempt.
        base = (0, 0)
    session, tokens, calls = _meter(run_dir, base, (session, tokens, calls))
    out.seek(0)
    tail = out.read()[-2000:]
    if resume_lost:
        # The marker rides on the output tail because that is what a killed
        # run leaves an operator to read.
        tail = f"[resume-lost] {tail}"
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
