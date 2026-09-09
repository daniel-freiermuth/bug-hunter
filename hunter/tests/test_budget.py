"""Tests for budget capacity ramps and OmpScavengeBackend.decide()."""

from __future__ import annotations

import sqlite3
import time
from pathlib import Path

import pytest

import hunter.backends.omp_scavenge.capacity as budget_module
from hunter.backends.omp_scavenge.capacity import ramp_5h, ramp_7d, read_windows, retry_at_5h, retry_at_7d
from hunter.backends.omp_scavenge.capacity import WindowState
from hunter.backends.omp_scavenge.facade import OmpScavengeBackend
from hunter.backend import Granted, Denied
from hunter.types import Config

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


class _FakeLedger:
    def __init__(self, running=0, finished=0):
        self._running = running
        self._finished = finished
    def running_estimate(self): return self._running
    def finished_since(self, ts_ms): return self._finished
    def finished_between(self, start, end): return 0
    def log_window_observation(self, *a, **kw): pass
    def last_window_observation(self, *a): return None
    def record_calibration_sample(self, *a, **kw): pass
    def estimate_capacity(self, *a, **kw): return None


def _backend(windows=None, ledger=None, monkeypatch=None, **cfg_kw):
    """Create an OmpScavengeBackend with controlled state."""
    import hunter.backends.omp_scavenge.capacity as cap
    if windows is not None and monkeypatch is not None:
        monkeypatch.setattr(cap, 'read_windows', lambda: windows)
    return OmpScavengeBackend(cfg=_cfg(**cfg_kw), ledger=ledger or _FakeLedger())


# ---------------------------------------------------------------------------
# Empty / stale → deny
# ---------------------------------------------------------------------------


def test_empty_windows_deny(monkeypatch):
    b = _backend(windows={}, monkeypatch=monkeypatch)
    outlook = b.decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "no window data" in outlook.normal.reason


def test_stale_5h_low_usage_allows_via_ramp_not_bypass(monkeypatch):
    """A stale-but-realistic 5h reading with low usage still allows -- not
    because staleness is special-cased, but because the ramp has
    genuinely grown past it by now (ramp is computed from live wall-clock
    time, independent of when the reading was taken)."""
    stale_age = 3600.0  # 1h old, well above default stale_after_s=1800
    resets_at = _NOW_MS + int(1.0 * 3600 * 1000)  # 4h into a live 5h window
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.10, resets_at=resets_at, age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=stale_age),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted)


def test_stale_5h_high_usage_still_denies(monkeypatch):
    """Regression: a stale (>30min old) 5h reading whose used_fraction is
    still ahead of the live ramp must keep denying -- staleness is no
    longer a bypass."""
    stale_age = 3600.0
    resets_at = _NOW_MS + int(1.0 * 3600 * 1000)  # 4h elapsed -> ramp ~0.778
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.90, resets_at=resets_at, age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=stale_age),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "5h" in outlook.normal.reason


def test_stale_5h_own_finished_jobs_count_toward_effective_used(monkeypatch):
    """Regression: tokens hunter's own jobs have already spent since the
    last probe must push effective_used up even while the raw reading
    itself is still fresh-looking and low."""
    resets_at = _NOW_MS + int(1.0 * 3600 * 1000)  # ramp ~0.778
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.10, resets_at=resets_at, age_s=30.0),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=30.0),
    }
    # 1.6M unaccounted tokens -> 1.6M/200k * 10% = 0.80 additional effective usage.
    ledger = _FakeLedger(finished=1_600_000)
    outlook = _backend(windows=windows, ledger=ledger, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "5h" in outlook.normal.reason


def test_stale_5h_denied_by_7d_ramp(monkeypatch):
    """Stale 5h but 7d over ramp → deny."""
    stale_age = 3600.0
    resets_at = _NOW_MS + int(_WEEK_MS * 0.95)  # 5% elapsed
    windows = {
        "anthropic:5h": _ws("anthropic:5h", age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.30, resets_at=resets_at, age_s=stale_age),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "7d" in outlook.normal.reason


def test_5h_and_7d_unaccounted_reservations_are_independent(monkeypatch):
    """Prove that 5h and 7d unaccounted reservations are independent:
    a huge finished-tokens amount denies via 7d even when 5h is healthy,
    and vice versa. The fake ledger returns the same finished amount for
    both windows, but the facade's scaling differs per dimension, so a
    large enough amount trips 7d (which has a much tighter ramp early)."""
    # Big finished amount -> 7d denial (7d ramp at 10% elapsed ~= 0.10,
    # and 20M tokens scaled to 7d fraction is huge).
    resets_7d = _NOW_MS + int(_WEEK_MS * 0.90)  # 10% elapsed -> ramp ~0.10
    windows = _healthy_windows(w5_used=0.0, w5_elapsed_h=4.5)
    windows["anthropic:7d"] = _ws("anthropic:7d", used_fraction=0.02, resets_at=resets_7d)
    ledger = _FakeLedger(finished=20_000_000)
    outlook = _backend(windows=windows, ledger=ledger, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "7d" in outlook.normal.reason

    # Big finished amount -> 5h denial (5h is mid-harvest, ramp ~0.556).
    windows2 = _healthy_windows(w5_used=0.0, w5_elapsed_h=3.0)
    ledger2 = _FakeLedger(finished=5_000_000)
    outlook2 = _backend(windows=windows2, ledger=ledger2, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook2.normal, Denied)
    assert "5h" in outlook2.normal.reason


# ---------------------------------------------------------------------------
# 7d ramp → deny
# ---------------------------------------------------------------------------


def test_7d_used_above_ramp_deny(monkeypatch):
    """7d used_fraction exceeds linear ramp -> deny."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.90)  # 10% elapsed
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d"] = _ws(
        "anthropic:7d", used_fraction=0.30, resets_at=resets_at,
    )
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "ramp" in outlook.normal.reason
    assert outlook.normal.retry_at == pytest.approx(_NOW_MS + 0.20 * _WEEK_MS, abs=2000)


def test_7d_used_below_ramp_allow(monkeypatch):
    """7d used_fraction below ramp -> OK (5h also ok)."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.50)  # 50% elapsed
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d"] = _ws(
        "anthropic:7d", used_fraction=0.30, resets_at=resets_at,
    )
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted)


# ---------------------------------------------------------------------------
# 5h last-hour ramp
# ---------------------------------------------------------------------------


def test_5h_first_30min_deny(monkeypatch):
    """Within the first 30 minutes, the 5h ramp is 0 → deny."""
    windows = _healthy_windows(w5_used=0.05, w5_elapsed_h=0.25)
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "5h" in outlook.normal.reason
    assert "ramp" in outlook.normal.reason


def test_5h_at_exactly_30min_deny(monkeypatch):
    """At exactly 30min elapsed, ramp is 0 → any usage > 0 denies."""
    windows = _healthy_windows(w5_used=0.01, w5_elapsed_h=0.5)
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)


def test_5h_harvest_halfway_low_usage_allow(monkeypatch):
    """2.75h elapsed (halfway through 4.5h harvest) → ramp = 0.5; usage 0.05 < 0.5 → allow."""
    windows = _healthy_windows(w5_used=0.05, w5_elapsed_h=2.75)
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted)

def test_5h_harvest_halfway_high_usage_deny(monkeypatch):
    """2.75h elapsed → ramp = 0.5; usage 0.60 ≥ 0.5 → deny."""
    windows = _healthy_windows(w5_used=0.60, w5_elapsed_h=2.75)
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "5h" in outlook.normal.reason
    assert "ramp" in outlook.normal.reason
    assert outlook.normal.retry_at == pytest.approx(_NOW_MS + 0.45 * 3600 * 1000, abs=2000)

def test_5h_harvest_end_high_usage_allow(monkeypatch):
    """4.95h elapsed → ramp ≈ 0.989; usage 0.90 < 0.989 → allow."""
    windows = _healthy_windows(w5_used=0.90, w5_elapsed_h=4.95)
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted)


def test_5h_exhausted_deny(monkeypatch):
    """Exhausted 5h window → deny regardless of timing."""
    windows = _healthy_windows(w5_elapsed_h=4.5)
    resets_at = _NOW_MS + _WEEK_MS // 2
    windows["anthropic:5h"] = _ws(
        "anthropic:5h", used_fraction=1.0, status="exhausted", resets_at=resets_at,
    )
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert outlook.normal.retry_at == resets_at


def test_5h_exhausted_but_stale_still_denies(monkeypatch):
    """Regression: an "exhausted" reading older than stale_after_s must NOT
    be treated as "no active window" (opener-safe). resets_at is still in
    the future, so the window is definitely still exhausted."""
    stale_age = 3600.0  # well above default stale_after_s=1800
    resets_at = _NOW_MS + 4 * 60 * 1000  # reset is still 4 minutes away
    windows = {
        "anthropic:5h": _ws(
            "anthropic:5h", used_fraction=1.0, status="exhausted",
            resets_at=resets_at, age_s=stale_age,
        ),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=stale_age),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert outlook.normal.retry_at == resets_at


def test_7d_denial_during_5h_headroom_uses_7d_retry_not_5h_timing(monkeypatch):
    """Regression: a 7d-ramp denial that happens to coincide with the 5h
    window's initial headroom period must report a retry_at based on the
    7d ramp."""
    resets_5h = _NOW_MS + int(4.75 * 3600 * 1000)  # 15min into a fresh 5h window
    resets_7d = _NOW_MS + int(_WEEK_MS * 0.90)  # 10% elapsed into the 7d window
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.0, resets_at=resets_5h),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.30, resets_at=resets_7d),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert outlook.normal.reason.startswith("anthropic:7d")
    assert outlook.normal.retry_at == pytest.approx(_NOW_MS + 0.20 * _WEEK_MS, abs=2000)


# ---------------------------------------------------------------------------
# No active 5h window → allow (opens one)
# ---------------------------------------------------------------------------


def test_no_5h_window_allow(monkeypatch):
    """No 5h window in data → allow, gated only by 7d ramp."""
    windows = {
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted)
    assert outlook.normal.cap_tokens is not None and outlook.normal.cap_tokens > 0


def test_no_5h_window_but_7d_over_deny(monkeypatch):
    """No 5h window, but 7d ramp exceeded → deny."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.95)  # 5% elapsed
    windows = {
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.30, resets_at=resets_at),
    }
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "7d" in outlook.normal.reason


# ---------------------------------------------------------------------------
# Expired-cycle 7d windows (e.g. a per-model-class window nobody has
# probed since the config stopped using that model) must not gate on
# ancient data from an already-completed week.
# ---------------------------------------------------------------------------


def test_expired_model_class_window_ignored(monkeypatch):
    """A :7d:<model> window whose resets_at is weeks in the past (model no
    longer in use, never re-probed) must be excluded from gating."""
    stale_age = 26 * 86400.0
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d:abandoned-model"] = _ws(
        "anthropic:7d:abandoned-model",
        used_fraction=0.99,
        resets_at=_NOW_MS - int(3 * _WEEK_MS),
        age_s=stale_age,
    )
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted), outlook.normal.reason


def test_active_model_class_window_still_gates(monkeypatch):
    """A :7d:<model> window that IS current (resets_at in the future) must
    still gate normally."""
    resets_at = _NOW_MS + int(_WEEK_MS * 0.95)  # 5% elapsed
    windows = _healthy_windows(w5_elapsed_h=4.5)
    windows["anthropic:7d:active-model"] = _ws(
        "anthropic:7d:active-model", used_fraction=0.30, resets_at=resets_at,
    )
    outlook = _backend(windows=windows, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "7d" in outlook.normal.reason

# ---------------------------------------------------------------------------
# Healthy → allow, correct cap
# ---------------------------------------------------------------------------


def test_healthy_allow(monkeypatch):
    """Healthy windows → Granted with positive cap_tokens."""
    outlook = _backend(windows=_healthy_windows(), monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Granted)
    assert outlook.normal.cap_tokens is not None and outlook.normal.cap_tokens > 0


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


def test_read_windows_rolls_forward_expired_5h_window(tmp_path, monkeypatch):
    """Regression: a real production incident. anthropic:5h's own cycle
    ended (resets_at passed) and no fresh probe had landed yet for the new
    cycle -- the old behavior dropped the row entirely, decide() saw no
    anthropic:5h data at all, and fell through to "no active window ->
    allow", completely bypassing unaccounted_tokens for as long as the gap
    lasted. A cascade of modernization jobs (which routinely cost 2-5x
    their nominal cap on a cold-cache first call) burned ~2.9M tokens
    across the freshly-rolled-over window with zero denials.

    Fixed: the window must be rolled forward, not dropped -- a brand-new
    cycle legitimately starts at 0% (unlike a truly abandoned per-model
    dimension, see test_read_windows_drops_expired_cycle_window), and
    recorded_at must be the boundary the new cycle actually started at (the
    old resets_at), not `now` -- otherwise unaccounted_tokens's "finished
    since the last probe" query would only count jobs from this instant
    forward instead of the whole gap."""
    db_path = tmp_path / "agent.db"
    now = int(time.time() * 1000)
    old_resets_at = now - 47 * 60 * 1000  # cycle ended 47 minutes ago
    _make_agent_db(
        db_path,
        [("anthropic:5h", 0.36, "ok", old_resets_at, old_resets_at - 3600_000)],
    )
    monkeypatch.setattr(budget_module, "OMP_AGENT_DB", db_path)

    windows = read_windows()

    assert "anthropic:5h" in windows, "expired window must be rolled forward, not dropped"
    w5 = windows["anthropic:5h"]
    assert w5.used_fraction == 0.0
    assert w5.status == "ok"
    assert w5.resets_at == old_resets_at + _5H_MS
    assert w5.recorded_at == old_resets_at  # the new cycle's actual start


def test_read_windows_rolls_forward_expired_7d_window(tmp_path, monkeypatch):
    """Same fix, the account-wide anthropic:7d dimension -- NOT the
    per-model-class variants (see test_read_windows_drops_expired_cycle_window,
    which must keep dropping those)."""
    db_path = tmp_path / "agent.db"
    now = int(time.time() * 1000)
    old_resets_at = now - 2 * 3600_000  # cycle ended 2 hours ago
    _make_agent_db(
        db_path,
        [("anthropic:7d", 0.55, "ok", old_resets_at, old_resets_at - _WEEK_MS)],
    )
    monkeypatch.setattr(budget_module, "OMP_AGENT_DB", db_path)

    windows = read_windows()

    assert "anthropic:7d" in windows
    w7 = windows["anthropic:7d"]
    assert w7.used_fraction == 0.0
    assert w7.resets_at == old_resets_at + _WEEK_MS
    assert w7.recorded_at == old_resets_at


def test_read_windows_rolls_forward_through_multiple_missed_cycles(tmp_path, monkeypatch):
    """If MULTIPLE cycles have elapsed since the last probe (a long outage,
    not just one rollover), the synthesized window must land on the
    CURRENT cycle, not the first one after the stale reading."""
    db_path = tmp_path / "agent.db"
    now = int(time.time() * 1000)
    old_resets_at = now - int(2.3 * _5H_MS)  # ~2.3 windows' worth stale
    _make_agent_db(
        db_path,
        [("anthropic:5h", 0.80, "ok", old_resets_at, old_resets_at - _5H_MS)],
    )
    monkeypatch.setattr(budget_module, "OMP_AGENT_DB", db_path)

    windows = read_windows()

    w5 = windows["anthropic:5h"]
    assert w5.resets_at > now
    assert w5.resets_at - now <= _5H_MS  # exactly one window ahead of now, not further
    assert w5.used_fraction == 0.0


def test_decide_denies_on_unaccounted_alone_through_a_fresh_rollover(monkeypatch):
    """End-to-end regression: a 5h window that just rolled over
    (used_fraction=0.0) must still deny once hunter's own unaccounted
    spend alone crosses the ramp."""
    elapsed_h = 3.0
    resets_at = _NOW_MS + int((5 - elapsed_h) * 3600 * 1000)
    window_start = resets_at - _5H_MS
    windows = {
        "anthropic:5h": WindowState(
            limit_id="anthropic:5h",
            used_fraction=0.0,
            status="ok",
            resets_at=resets_at,
            recorded_at=window_start,
            age_s=(_NOW_MS - window_start) / 1000,
        ),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10),
    }
    # ~2.9M tokens finished since the rollover.
    ledger = _FakeLedger(finished=2_900_000)
    outlook = _backend(windows=windows, ledger=ledger, monkeypatch=monkeypatch).decide(anticipated_tokens=0)
    assert isinstance(outlook.normal, Denied)
    assert "5h" in outlook.normal.reason


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


class TestRetryAt7d:
    def test_no_resets_at_returns_none(self):
        assert retry_at_7d(None, 0.5) is None

    def test_is_the_exact_inverse_of_ramp_7d(self):
        """retry_at_7d(resets_at, u) is the timestamp t at which
        ramp_7d(resets_at, t) == u -- verify the round trip directly
        rather than trusting the algebra by eye."""
        resets_at = _NOW_MS + int(_WEEK_MS * 0.4)
        for u in (0.0, 0.1, 0.5, 0.9):
            t = retry_at_7d(resets_at, u)
            assert t is not None
            assert ramp_7d(resets_at, t) == pytest.approx(u, abs=1e-9)


class TestRetryAt5h:
    def test_no_resets_at_returns_none(self):
        assert retry_at_5h(None, 0.5) is None

    def test_is_the_exact_inverse_of_ramp_5h(self):
        resets_at = _NOW_MS + int(3.2 * 3600 * 1000)
        for u in (0.0, 0.25, 0.5, 0.9):
            t = retry_at_5h(resets_at, u)
            assert t is not None
            assert ramp_5h(resets_at, t) == pytest.approx(u, abs=1e-9)

    def test_zero_used_lands_at_headroom_end(self):
        resets_at = _NOW_MS + int(4 * 3600 * 1000)  # window started 1h ago
        window_start = resets_at - _5H_MS
        t = retry_at_5h(resets_at, 0.0)
        assert t == pytest.approx(window_start + 30 * 60 * 1000)
