"""Tests for hunter.budget.decide()."""

from __future__ import annotations

import time
from pathlib import Path

from hunter.budget import decide
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
