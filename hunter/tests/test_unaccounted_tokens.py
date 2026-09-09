"""Tests for OmpScavengeBackend._unaccounted_fraction.

budget.decide() trusts each window's latest probe used_fraction as a
floor and relies on this method to add whatever hunter's own job
history knows has been spent since THAT window's own floor was measured
(Anthropic's probe is sparse and can lag hours behind an active run --
see capacity.read_windows and its own tests for the window-rollover half
of this story).

Returns a (reservation_5h, reservation_7d) tuple of fraction reservations,
each independently computed against its own window's recorded_at.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter.backends.omp_scavenge.capacity import WindowState
from hunter.backends.omp_scavenge.facade import (
    OmpScavengeBackend,
    _5H_7D_RATIO,
    _TOK_PER_FRAC_5H,
)
from hunter.store import Store
from hunter.types import Config, now_ms


@pytest.fixture
def store(tmp_path: Path) -> Store:
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    return Store(cfg)


@pytest.fixture
def backend(store: Store, tmp_path: Path) -> OmpScavengeBackend:
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    return OmpScavengeBackend(cfg=cfg, ledger=store)


def _ws(limit_id: str, recorded_at: int) -> WindowState:
    return WindowState(
        limit_id=limit_id,
        used_fraction=0.1,
        status="ok",
        resets_at=recorded_at + 3600_000,
        recorded_at=recorded_at,
    )


def _finished_job(store: Store, repo_id: int, tokens: int, finished_at: int) -> None:
    jid = store.create_job("dep_update", repo_id)
    store.update_job(jid, state="done", tokens_new=tokens, finished_at=finished_at)


def _running_job(store: Store, repo_id: int, cap_tokens: int) -> None:
    store.create_job("hunt", repo_id, cap_tokens=cap_tokens, state="running")


def _backing_tokens(res_5h: float, res_7d: float) -> tuple[int, int]:
    """Convert reservation fractions back to the underlying token counts.

    Inverse of the conversion in _unaccounted_fraction:
      res_5h = unaccounted_5h / _TOK_PER_FRAC_5H
      res_7d = unaccounted_7d / _TOK_PER_FRAC_5H * _5H_7D_RATIO
    """
    tok_5h = round(res_5h * _TOK_PER_FRAC_5H)
    tok_7d = round(res_7d / _5H_7D_RATIO * _TOK_PER_FRAC_5H) if res_7d else 0
    return tok_5h, tok_7d


def test_no_jobs_returns_zero(store: Store, backend: OmpScavengeBackend) -> None:
    windows = {
        "anthropic:5h": _ws("anthropic:5h", now_ms() - 3600_000),
        "anthropic:7d": _ws("anthropic:7d", now_ms() - 3600_000),
    }
    tok_5h, tok_7d = _backing_tokens(*backend._unaccounted_fraction(windows, 0))
    assert tok_5h == 0
    assert tok_7d == 0


def test_running_job_counted_via_cap_tokens_in_both_fields(
    store: Store, backend: OmpScavengeBackend,
) -> None:
    """A still-running job's estimated cost is unaccounted-for from
    EITHER window's perspective equally -- it hasn't finished, so no
    probe reflects it yet regardless of which dimension is asked."""
    rid = store.add_repo("r", "https://r", "/r")
    _running_job(store, rid, cap_tokens=150_000)
    windows = {
        "anthropic:5h": _ws("anthropic:5h", now_ms() - 3600_000),
        "anthropic:7d": _ws("anthropic:7d", now_ms() - 3600_000),
    }
    tok_5h, tok_7d = _backing_tokens(*backend._unaccounted_fraction(windows, 0))
    assert tok_5h == 150_000
    assert tok_7d == 150_000


def test_anticipated_added_to_both_fields(
    store: Store, backend: OmpScavengeBackend,
) -> None:
    """The about-to-run job's own anticipated cost applies identically
    to both windows -- it doesn't depend on any probe baseline."""
    windows = {
        "anthropic:5h": _ws("anthropic:5h", now_ms() - 3600_000),
        "anthropic:7d": _ws("anthropic:7d", now_ms() - 3600_000),
    }
    tok_5h, tok_7d = _backing_tokens(*backend._unaccounted_fraction(windows, anticipated=80_000))
    assert tok_5h == 80_000
    assert tok_7d == 80_000


def test_finished_job_scoped_to_each_windows_own_probe(
    store: Store, backend: OmpScavengeBackend,
) -> None:
    """A job that finished after BOTH windows' probes counts toward
    both; a job finished before either window's own probe does not
    count toward that specific window."""
    rid = store.add_repo("r", "https://r", "/r")
    probe_5h_at = now_ms() - 3600_000
    probe_7d_at = now_ms() - 7200_000  # 7d probed earlier than 5h
    _finished_job(store, rid, tokens=50_000, finished_at=probe_5h_at + 60_000)
    windows = {
        "anthropic:5h": _ws("anthropic:5h", probe_5h_at),
        "anthropic:7d": _ws("anthropic:7d", probe_7d_at),
    }
    tok_5h, tok_7d = _backing_tokens(*backend._unaccounted_fraction(windows, 0))
    assert tok_5h == 50_000
    assert tok_7d == 50_000  # also after 7d's (earlier) probe


def test_stale_7d_probe_no_longer_drags_the_5h_baseline_back(
    store: Store, backend: OmpScavengeBackend,
) -> None:
    """Reproduces a real production regression, caught live the same day
    the 5h-rollover fix shipped: immediately before and immediately after
    a 5h window rolled over, the logged "unaccounted" figure was IDENTICAL
    to two decimal places, because a SHARED probe_at never actually moved
    -- anthropic:7d's much older, unrefreshed recorded_at kept winning a
    min() across both windows.

    Shape: a job finished BEFORE the 5h window rolled over (it belongs to,
    and was already gated against, the window that just ended) but AFTER
    7d's last real probe (much earlier -- 7d hadn't rolled over). It must
    NOT count toward for_5h (double-charging spend the ended window's own
    ramp already accounted for) but MUST count toward for_7d (it genuinely
    postdates 7d's own last probe, so from 7d's perspective it really is
    unaccounted-for)."""
    rid = store.add_repo("r", "https://r", "/r")
    seven_d_probe_at = now_ms() - 3 * 3600_000  # 7d's last real probe, 3h ago
    five_h_rollover_at = now_ms() - 3600_000  # 5h window rolled over 1h ago

    _finished_job(store, rid, tokens=999_999, finished_at=five_h_rollover_at - 60_000)

    windows = {
        "anthropic:5h": _ws("anthropic:5h", five_h_rollover_at),
        "anthropic:7d": _ws("anthropic:7d", seven_d_probe_at),
    }
    tok_5h, tok_7d = _backing_tokens(*backend._unaccounted_fraction(windows, 0))
    assert tok_5h == 0, (
        "a job that finished before the 5h rollover must not count against the new "
        "5h window just because anthropic:7d's probe is still older"
    )
    assert tok_7d == 999_999, (
        "the same job DOES postdate 7d's own last probe, so it must count toward "
        "for_7d -- excluding it there too (the pre-fix behavior) silently "
        "under-reserves the 7d ramp"
    )

    # A job finishing after the 5h rollover boundary counts toward both.
    _finished_job(store, rid, tokens=42_000, finished_at=five_h_rollover_at + 60_000)
    tok_5h2, tok_7d2 = _backing_tokens(*backend._unaccounted_fraction(windows, 0))
    assert tok_5h2 == 42_000
    assert tok_7d2 == 999_999 + 42_000


def test_falls_back_to_min_when_window_missing(
    store: Store, backend: OmpScavengeBackend,
) -> None:
    """No anthropic:5h (or anthropic:7d) entry at all -> that field falls
    back to min() across whatever IS present, rather than crashing or
    silently ignoring everything."""
    rid = store.add_repo("r", "https://r", "/r")
    probe_at = now_ms() - 3600_000
    _finished_job(store, rid, tokens=10_000, finished_at=probe_at + 30_000)
    windows = {"anthropic:7d": _ws("anthropic:7d", probe_at)}
    tok_5h, tok_7d = _backing_tokens(*backend._unaccounted_fraction(windows, 0))
    assert tok_5h == 10_000  # falls back to the only window present
    assert tok_7d == 10_000
