"""The watchdog's two stop conditions, and what happens when one is absent.

A granted job's token cap is whatever headroom the budget ramp had left,
so a grant can legitimately carry no figure at all.  ``None`` there means
no token bound -- not "zero" and not "some large number" -- and the
wall-clock limit is then the only thing that stops a runaway worker.
"""

from __future__ import annotations

from pathlib import Path

from hunter.backends.omp_scavenge import harness
from hunter.types import Config

# One assistant call, far past any cap anyone would configure.
_HUGE_TOKENS = 10_000_000


def _fake_omp(tmp_path: Path, *, sleep_s: float) -> Path:
    """An omp that meters huge immediately, then stays alive.

    It has to still be running once the ledger is on disk, or the loop
    breaks on the exit code before it ever evaluates the token check.
    """
    script = tmp_path / "fake-omp"
    record = (
        '{"message":{"role":"assistant","usage":'
        f'{{"input":{_HUGE_TOKENS},"output":0,"cacheWrite":0}}}}}}'
    )
    script.write_text(
        "#!/bin/sh\n"
        'for a in "$@"; do\n'
        '  case "$a" in --session-dir=*) d="${a#--session-dir=}" ;; esac\n'
        "done\n"
        f"printf '%s\\n' '{record}' > \"$d/s.jsonl\"\n"
        f"sleep {sleep_s}\n"
        "exit 0\n"
    )
    script.chmod(0o755)
    return script


def _cfg(tmp_path: Path, omp: Path) -> Config:
    return Config(
        work_root=tmp_path / "wr",
        db_path=tmp_path / "wr" / "t.db",
        omp_bin=str(omp),
        poll_s=0.05,
    )


def test_no_token_cap_means_no_token_kill(tmp_path: Path) -> None:
    """The worker meters 10M new tokens -- past every cap this daemon has
    ever been configured with -- and must run to its own exit."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, sleep_s=2))

    rr = harness.run_worker(cfg, cwd, "prompt", cap_tokens=None, max_wall_s=60)

    assert rr.tokens_new == _HUGE_TOKENS, "the watchdog must still meter an uncapped worker"
    assert rr.killed_reason is None, f"an uncapped worker was killed: {rr.killed_reason}"
    assert rr.exit_code == 0


def test_wallclock_still_stops_a_worker_with_no_token_cap(tmp_path: Path) -> None:
    """Dropping the token bound leaves max_wall_s as the whole defence
    against a worker that never exits."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, sleep_s=60))

    rr = harness.run_worker(cfg, cwd, "prompt", cap_tokens=None, max_wall_s=1)

    assert rr.killed_reason == "wallclock"
    assert rr.duration_s < 30, "the wall-clock kill must not wait for the worker"


def test_a_finite_cap_still_kills(tmp_path: Path) -> None:
    """The bound is dropped only when the grant carries none."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, sleep_s=60))

    rr = harness.run_worker(cfg, cwd, "prompt", cap_tokens=1000, max_wall_s=60)

    assert rr.killed_reason == "cap"
