"""Tests for hunter.budget.decide() and hunter.budget.read_windows()."""

from __future__ import annotations

import sqlite3
import time
from pathlib import Path

import pytest

import hunter.budget as budget_module
from hunter.budget import decide, ramp_5h, ramp_7d, read_windows
from hunter.types import Config, WindowState

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_NOW_MS = int(time.time() * 1000)
_WEEK_MS = 7 * 24 * 3600 * 1000
_5H_MS = 5 * 3600 * 1000
_1H_MS = 1 * 3600 * 1000


def _cfg(**overrides) -> Config:
    defaults = {
        "work_root": Path("/tmp"),
        "db_path": Path("/tmp/test.db"),
        "hunt_cap_tokens": 200_000,
        "fix_cap_tokens": 150_000,
        "stale_after_s": 1800,
    }
    defaults.update(overrides)
    return Config(**defaults)


def _ws(
    limit_id: str,
    *,
    used_fraction: float | None = 0.10,
    status: str | None = "ok",
    resets_at: int | None = None,
    age_s: float = 60.0,
) -> WindowState:
    """Build a WindowState with sensible defaults (fresh, low usage)."""
    return WindowState(
        limit_id=limit_id,
        used_fraction=used_fraction,
        status=status,
        resets_at=resets_at if resets_at is not None else _NOW_MS + _WEEK_MS // 2,
        recorded_at=_NOW_MS - int(age_s * 1000),
        age_s=age_s,
    )


def _healthy_windows(
    *,
    w5_used: float = 0.05,
    w5_elapsed_h: float = 4.5,
) -> dict[str, WindowState]:
    """Windows that should produce an allow decision.

    Default: 5h window halfway through the harvest hour, low usage.
    """
    resets_5h = _NOW_MS + int((5 - w5_elapsed_h) * 3600 * 1000)
    return {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=w5_used, resets_at=resets_5h),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10),
        "anthropic:7d:model-class": _ws("anthropic:7d:model-class", used_fraction=0.10),
    }


# ---------------------------------------------------------------------------
# Empty / stale → deny
# ---------------------------------------------------------------------------


def test_empty_windows_deny():
    d = decide(_cfg(), "hunt", {})
    assert not d.allow
    assert "no window data" in d.reason


def test_all_stale_5h_allows_as_opener():
    """Stale 5h data → treated as no active window → allow (7d still gates)."""
    stale_age = 3600.0  # well above default stale_after_s=1800
    windows = {
        "anthropic:5h": _ws("anthropic:5h", age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=stale_age),
    }
    d = decide(_cfg(), "hunt", windows)
    assert d.allow


def test_stale_5h_denied_by_7d_ramp():
    """Stale 5h but 7d over ramp → deny."""
    stale_age = 3600.0
    resets_at = _NOW_MS + int(_WEEK_MS * 0.95)  # 5% elapsed
    windows = {
        "anthropic:5h": _ws("anthropic:5h", age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.30, resets_at=resets_at, age_s=stale_age),
    }
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "7d" in d.reason


# ---------------------------------------------------------------------------
# 7d ramp → deny
# ---------------------------------------------------------------------------


def test_7d_used_above_ramp_deny():
    """7d used_fraction exceeds linear ramp -> deny."""
    # Place us 10% into the 7d window, but used_fraction = 0.30
    resets_at = _NOW_MS + int(_WEEK_MS * 0.90)  # 10% elapsed
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d"] = _ws(
        "anthropic:7d", used_fraction=0.30, resets_at=resets_at,
    )
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "ramp" in d.reason


def test_7d_used_below_ramp_allow():
    """7d used_fraction below ramp -> OK (5h also ok)."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.50)  # 50% elapsed
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d"] = _ws(
        "anthropic:7d", used_fraction=0.30, resets_at=resets_at,
    )
    d = decide(_cfg(), "hunt", windows)
    assert d.allow


# ---------------------------------------------------------------------------
# 5h last-hour ramp
# ---------------------------------------------------------------------------


def test_5h_first_30min_deny():
    """Within the first 30 minutes, the 5h ramp is 0 → deny."""
    windows = _healthy_windows(w5_used=0.05, w5_elapsed_h=0.25)
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "5h" in d.reason
    assert "harvest" in d.reason


def test_5h_at_exactly_30min_deny():
    """At exactly 30min elapsed, ramp is 0 → any usage > 0 denies."""
    windows = _healthy_windows(w5_used=0.01, w5_elapsed_h=0.5)
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow


def test_5h_harvest_halfway_low_usage_allow():
    """2.75h elapsed (halfway through 4.5h harvest) → ramp = 0.5; usage 0.05 < 0.5 → allow."""
    windows = _healthy_windows(w5_used=0.05, w5_elapsed_h=2.75)
    d = decide(_cfg(), "hunt", windows)
    assert d.allow

def test_5h_harvest_halfway_high_usage_deny():
    """2.75h elapsed → ramp = 0.5; usage 0.60 ≥ 0.5 → deny."""
    windows = _healthy_windows(w5_used=0.60, w5_elapsed_h=2.75)
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "5h" in d.reason
    assert "harvest" in d.reason

def test_5h_harvest_end_high_usage_allow():
    """4.95h elapsed → ramp ≈ 0.989; usage 0.90 < 0.989 → allow."""
    windows = _healthy_windows(w5_used=0.90, w5_elapsed_h=4.95)
    d = decide(_cfg(), "hunt", windows)
    assert d.allow


def test_5h_exhausted_deny():
    """Exhausted 5h window → deny regardless of timing."""
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:5h"] = _ws("anthropic:5h", used_fraction=1.0, status="exhausted")
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow


# ---------------------------------------------------------------------------
# No active 5h window → allow (opens one)
# ---------------------------------------------------------------------------


def test_no_5h_window_allow():
    """No 5h window in data → allow, gated only by 7d ramp."""
    windows = {
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10),
    }
    d = decide(_cfg(), "hunt", windows)
    assert d.allow
    assert d.cap_tokens == 200_000


def test_no_5h_window_but_7d_over_deny():
    """No 5h window, but 7d ramp exceeded → deny."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.95)  # 5% elapsed
    windows = {
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.30, resets_at=resets_at),
    }
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "7d" in d.reason


# ---------------------------------------------------------------------------
# Expired-cycle 7d windows (e.g. a per-model-class window nobody has
# probed since the config stopped using that model) must not gate on
# ancient data from an already-completed week.
# ---------------------------------------------------------------------------


def test_expired_model_class_window_ignored():
    """A :7d:<model> window whose resets_at is weeks in the past (model no
    longer in use, never re-probed) must be excluded from gating -- its
    used_fraction describes a bygone cycle, not now."""
    stale_age = 26 * 86400.0  # ~26 days, matching the observed production case
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d:abandoned-model"] = _ws(
        "anthropic:7d:abandoned-model",
        used_fraction=0.99,  # would deny everything if it were honored
        resets_at=_NOW_MS - int(3 * _WEEK_MS),
        age_s=stale_age,
    )
    d = decide(_cfg(), "hunt", windows)
    assert d.allow, d.reason


def test_active_model_class_window_still_gates():
    """A :7d:<model> window that IS current (resets_at in the future) must
    still gate normally -- the fix only excludes expired cycles, not every
    per-model-class window."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.95)  # 5% elapsed
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d:active-model"] = _ws(
        "anthropic:7d:active-model", used_fraction=0.30, resets_at=resets_at,
    )
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "7d" in d.reason

# ---------------------------------------------------------------------------
# Healthy → allow, correct cap
# ---------------------------------------------------------------------------


def test_healthy_allow_hunt():
    d = decide(_cfg(), "hunt", _healthy_windows())
    assert d.allow
    assert d.cap_tokens == 200_000


def test_healthy_allow_fix():
    d = decide(_cfg(), "fix", _healthy_windows())
    assert d.allow
    assert d.cap_tokens == 150_000


def test_kind_selects_base_cap():
    cfg = _cfg(hunt_cap_tokens=300_000, fix_cap_tokens=100_000)
    dh = decide(cfg, "hunt", _healthy_windows())
    df = decide(cfg, "fix", _healthy_windows())
    assert dh.cap_tokens == 300_000
    assert df.cap_tokens == 100_000


# ---------------------------------------------------------------------------
# read_windows(): expired-cycle windows must not even be surfaced
# ---------------------------------------------------------------------------


def _make_agent_db(path: Path, rows: list[tuple[str, float, str, int, int]]) -> None:
    """rows: (limit_id, used_fraction, status, resets_at, recorded_at)."""
    db = sqlite3.connect(path)
    db.execute(
        """CREATE TABLE usage_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            recorded_at INTEGER NOT NULL,
            provider TEXT NOT NULL,
            account_key TEXT NOT NULL,
            limit_id TEXT NOT NULL,
            label TEXT NOT NULL,
            used_fraction REAL,
            status TEXT,
            resets_at INTEGER
        )"""
    )
    for limit_id, used_fraction, status, resets_at, recorded_at in rows:
        db.execute(
            "INSERT INTO usage_history"
            " (recorded_at, provider, account_key, limit_id, label, used_fraction, status, resets_at)"
            " VALUES (?, 'anthropic', 'acct', ?, ?, ?, ?, ?)",
            (recorded_at, limit_id, limit_id, used_fraction, status, resets_at),
        )
    db.commit()
    db.close()


def test_read_windows_drops_expired_cycle_window(tmp_path, monkeypatch):
    """A window whose resets_at has already passed (e.g. anthropic:7d:fable,
    abandoned since hunter's config stopped routing to that model class)
    must not be surfaced at all -- not to decide(), not to the UI."""
    db_path = tmp_path / "agent.db"
    now = int(time.time() * 1000)
    _make_agent_db(
        db_path,
        [
            ("anthropic:7d", 0.3, "ok", now + _WEEK_MS // 2, now - 60_000),
            ("anthropic:7d:fable", 0.56, "ok", now - 26 * 86400_000, now - 26 * 86400_000),
        ],
    )
    monkeypatch.setattr(budget_module, "OMP_AGENT_DB", db_path)

    windows = read_windows()

    assert set(windows) == {"anthropic:7d"}


def test_read_windows_keeps_active_window(tmp_path, monkeypatch):
    db_path = tmp_path / "agent.db"
    now = int(time.time() * 1000)
    _make_agent_db(
        db_path,
        [("anthropic:7d", 0.3, "ok", now + _WEEK_MS // 2, now - 60_000)],
    )
    monkeypatch.setattr(budget_module, "OMP_AGENT_DB", db_path)

    windows = read_windows()

    assert set(windows) == {"anthropic:7d"}
    assert windows["anthropic:7d"].used_fraction == 0.3


# ---------------------------------------------------------------------------
# ramp_7d / ramp_5h: pure functions, now reused by both decide() (gating)
# and server._summary() (the "available budget" the UI displays).
# ---------------------------------------------------------------------------


class TestRamp7d:
    def test_no_resets_at_assumes_end_of_window(self):
        assert ramp_7d(None, _NOW_MS) == 1.0

    def test_expired_resets_at_assumes_end_of_window(self):
        assert ramp_7d(_NOW_MS - 1000, _NOW_MS) == 1.0

    def test_halfway_through_window(self):
        resets_at = _NOW_MS + _WEEK_MS // 2  # started WEEK_MS/2 ago
        assert ramp_7d(resets_at, _NOW_MS) == pytest.approx(0.5, abs=1e-6)

    def test_just_started(self):
        resets_at = _NOW_MS + _WEEK_MS  # reset a full week out -> just started
        assert ramp_7d(resets_at, _NOW_MS) == pytest.approx(0.0, abs=1e-6)

    def test_clamped_to_one(self):
        # resets_at implies the window "started" in the future (bad data) --
        # must never exceed 1.0.
        resets_at = _NOW_MS + _WEEK_MS * 2
        assert ramp_7d(resets_at, _NOW_MS) <= 1.0


class TestRamp5h:
    def test_no_resets_at_returns_none(self):
        assert ramp_5h(None, _NOW_MS) is None

    def test_expired_resets_at_returns_none(self):
        assert ramp_5h(_NOW_MS - 1000, _NOW_MS) is None

    def test_within_headroom_is_zero(self):
        resets_at = _NOW_MS + (_5H_MS - int(15 * 60 * 1000))  # 15min elapsed
        assert ramp_5h(resets_at, _NOW_MS) == 0.0

    def test_halfway_through_harvest_ramp(self):
        # 2.75h elapsed of a 5h window (30min headroom -> 4.5h harvest) -> 50%
        resets_at = _NOW_MS + (_5H_MS - int(2.75 * 3600 * 1000))
        assert ramp_5h(resets_at, _NOW_MS) == pytest.approx(0.5, abs=1e-6)

    def test_never_negative(self):
        resets_at = _NOW_MS + _5H_MS  # just started -> before headroom ends
        assert ramp_5h(resets_at, _NOW_MS) == 0.0
