"""Tests for hunter.scheduler._unaccounted_tokens.

budget.decide() trusts the latest probe's used_fraction as a floor and
relies on this function to add whatever hunter's own job history knows
has been spent since that floor was measured (Anthropic's probe is sparse
and can lag hours behind an active run -- see budget.read_windows and its
own tests for the window-rollover half of this story).

probe_at anchors on anthropic:5h's own recorded_at specifically, not
min() across every window -- see this file's regression test for exactly
why: anthropic:7d rolls over far less often than 5h, so after a 5h
rollover it usually still carries an OLDER recorded_at from its own last
real probe, and min() would let that drag the "since the last probe"
baseline back to before the new 5h window even started -- silently
double-counting jobs that already belonged to (and were already gated
against) the window that just ended.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter.scheduler import _unaccounted_tokens
from hunter.store import Store
from hunter.types import Config, WindowState, now_ms


@pytest.fixture
def store(tmp_path: Path) -> Store:
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    return Store(cfg)


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


def test_no_jobs_returns_zero(store: Store) -> None:
    windows = {"anthropic:5h": _ws("anthropic:5h", now_ms() - 3600_000)}
    assert _unaccounted_tokens(store, windows) == 0


def test_running_job_counted_via_cap_tokens(store: Store) -> None:
    rid = store.add_repo("r", "https://r", "/r")
    _running_job(store, rid, cap_tokens=150_000)
    windows = {"anthropic:5h": _ws("anthropic:5h", now_ms() - 3600_000)}
    assert _unaccounted_tokens(store, windows) == 150_000


def test_finished_job_after_probe_counted(store: Store) -> None:
    rid = store.add_repo("r", "https://r", "/r")
    probe_at = now_ms() - 3600_000
    _finished_job(store, rid, tokens=50_000, finished_at=probe_at + 60_000)
    windows = {"anthropic:5h": _ws("anthropic:5h", probe_at)}
    assert _unaccounted_tokens(store, windows) == 50_000


def test_finished_job_before_probe_not_counted(store: Store) -> None:
    rid = store.add_repo("r", "https://r", "/r")
    probe_at = now_ms() - 3600_000
    _finished_job(store, rid, tokens=50_000, finished_at=probe_at - 60_000)
    windows = {"anthropic:5h": _ws("anthropic:5h", probe_at)}
    assert _unaccounted_tokens(store, windows) == 0


def test_stale_7d_probe_does_not_drag_baseline_back_after_5h_rollover(store: Store) -> None:
    """Reproduces a real production regression, caught live the same day
    the 5h-rollover fix shipped: immediately before and immediately after
    a 5h window rolled over, the logged "unaccounted" figure was IDENTICAL
    to two decimal places (0.67), because probe_at never actually moved --
    anthropic:7d's much older, unrefreshed recorded_at (from the same
    stale probe both windows last shared) kept winning the min().

    Shape: a job finished BEFORE the 5h window rolled over (it belongs to,
    and was already gated against, the window that just ended) but AFTER
    7d's last real probe. It must NOT be counted here -- counting it
    double-charges spend that the ended window's own used_fraction ramp
    already accounted for, needlessly delaying admission into the new
    window for exactly as long as 7d happens to stay unprobed."""
    rid = store.add_repo("r", "https://r", "/r")
    seven_d_probe_at = now_ms() - 3 * 3600_000  # 7d's last real probe, 3h ago
    five_h_rollover_at = now_ms() - 3600_000  # 5h window rolled over 1h ago

    # Belongs to the window that just ended: after 7d's stale probe, but
    # before the 5h rollover boundary.
    _finished_job(store, rid, tokens=999_999, finished_at=five_h_rollover_at - 60_000)

    windows = {
        "anthropic:5h": _ws("anthropic:5h", five_h_rollover_at),
        "anthropic:7d": _ws("anthropic:7d", seven_d_probe_at),
    }
    assert _unaccounted_tokens(store, windows) == 0, (
        "a job that finished before the 5h rollover must not be counted against "
        "the new window just because anthropic:7d's probe is still older"
    )

    # A job that finishes AFTER the rollover boundary must still count.
    _finished_job(store, rid, tokens=42_000, finished_at=five_h_rollover_at + 60_000)
    assert _unaccounted_tokens(store, windows) == 42_000


def test_falls_back_to_min_when_5h_window_missing(store: Store) -> None:
    """No anthropic:5h entry at all (e.g. genuinely missing data) -> fall
    back to the old min()-across-windows behavior rather than crashing or
    silently ignoring everything."""
    rid = store.add_repo("r", "https://r", "/r")
    probe_at = now_ms() - 3600_000
    _finished_job(store, rid, tokens=10_000, finished_at=probe_at + 30_000)
    windows = {"anthropic:7d": _ws("anthropic:7d", probe_at)}
    assert _unaccounted_tokens(store, windows) == 10_000
