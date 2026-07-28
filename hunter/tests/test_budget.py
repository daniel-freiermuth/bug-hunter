"""Tests for hunter.budget.decide()."""

from __future__ import annotations

import time
from pathlib import Path

from hunter.budget import _WEEK_MS, decide
from hunter.types import Config, WindowState

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_NOW_MS = int(time.time() * 1000)


def _cfg(**overrides) -> Config:
    defaults = {
        "work_root": Path("/tmp"),
        "db_path": Path("/tmp/test.db"),
        "hunt_cap_tokens": 200_000,
        "fix_cap_tokens": 150_000,
        "deny_5h_above": 0.85,
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


def _healthy_windows() -> dict[str, WindowState]:
    """Windows that should produce an allow decision."""
    return {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.20),
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


def test_all_stale_windows_deny():
    stale_age = 3600.0  # well above default stale_after_s=1800
    windows = {
        "anthropic:5h": _ws("anthropic:5h", age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", age_s=stale_age),
    }
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "stale" in d.reason


# ---------------------------------------------------------------------------
# 5h gate → deny
# ---------------------------------------------------------------------------


def test_5h_exhausted_deny():
    windows = _healthy_windows()
    windows["anthropic:5h"] = _ws("anthropic:5h", used_fraction=0.50, status="exhausted")
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "5h" in d.reason


def test_5h_above_deny_fraction():
    windows = _healthy_windows()
    windows["anthropic:5h"] = _ws("anthropic:5h", used_fraction=0.90)
    d = decide(_cfg(deny_5h_above=0.85), "hunt", windows)
    assert not d.allow
    assert "5h" in d.reason


def test_5h_at_exact_threshold_deny():
    windows = _healthy_windows()
    windows["anthropic:5h"] = _ws("anthropic:5h", used_fraction=0.85)
    d = decide(_cfg(deny_5h_above=0.85), "hunt", windows)
    assert not d.allow


# ---------------------------------------------------------------------------
# 7d ramp → deny
# ---------------------------------------------------------------------------


def test_7d_used_above_ramp_deny():
    """7d used_fraction exceeds linear ramp -> deny."""
    cfg = _cfg()
    # resets_at at midpoint -> elapsed_frac ~ 0.5, allowed ~ 0.5
    # used = 0.55 -> clearly above ramp -> deny
    windows = _healthy_windows()
    windows["anthropic:7d"] = _ws(
        "anthropic:7d",
        used_fraction=0.55,
        resets_at=_NOW_MS + _WEEK_MS // 2,
    )
    d = decide(cfg, "hunt", windows)
    assert not d.allow
    assert "ramp" in d.reason


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
