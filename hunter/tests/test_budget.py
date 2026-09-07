"""Tests for hunter.budget.decide() and hunter.budget.read_windows()."""

from __future__ import annotations

import sqlite3
import time
from pathlib import Path

import pytest

import hunter.budget as budget_module
from hunter.budget import decide, ramp_5h, ramp_7d, read_windows, retry_at_5h, retry_at_7d
from hunter.types import Config, UnaccountedTokens, WindowState

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


def test_stale_5h_low_usage_allows_via_ramp_not_bypass():
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
    d = decide(_cfg(), "hunt", windows)
    assert d.allow


def test_stale_5h_high_usage_still_denies():
    """Regression: a stale (>30min old) 5h reading whose used_fraction is
    still ahead of the live ramp must keep denying -- staleness is no
    longer a bypass. Reproduces the production incident: a probe recorded
    used=0.12 early in a window, then ~26 jobs ran back to back with zero
    fresh probes landing for 98 minutes (a common gap -- Anthropic's probe
    is sparse and doesn't track hunter's own job cadence); the old
    "stale ramp data -> treat as no window -> allow" bypass meant none of
    those jobs were gated at all until a probe finally landed, by which
    point usage had already blown from 12% to 69% against a ~50% ramp."""
    stale_age = 3600.0
    resets_at = _NOW_MS + int(1.0 * 3600 * 1000)  # 4h elapsed -> ramp ~0.778
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.90, resets_at=resets_at, age_s=stale_age),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=stale_age),
    }
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert "5h" in d.reason


def test_stale_5h_own_finished_jobs_count_toward_effective_used():
    """Regression: tokens hunter's own jobs have already spent since the
    last probe must push effective_used up even while the raw reading
    itself is still fresh-looking and low -- this is what actually closes
    the production gap (a plain staleness check wouldn't have caught it
    the moment the probe age crossed 30min; this catches it immediately,
    on the very next job, regardless of probe age)."""
    resets_at = _NOW_MS + int(1.0 * 3600 * 1000)  # ramp ~0.778
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.10, resets_at=resets_at, age_s=30.0),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=30.0),
    }
    # 1.6M unaccounted tokens (finished jobs the probe hasn't caught up to
    # yet) -> 1.6M/200k * 10% = 0.80 additional effective usage.
    d = decide(_cfg(), "hunt", windows, UnaccountedTokens(for_5h=1_600_000))
    assert not d.allow
    assert "5h" in d.reason


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


def test_5h_and_7d_unaccounted_reservations_are_independent():
    """The actual fix this session: for_5h and for_7d must never be
    derived from one another via a capacity ratio -- each window's
    reservation responds ONLY to its own field. A single shared int
    scaled down for 7d (the pre-fix design) silently assumed both
    windows' unaccounted spend shared one baseline, which is false the
    moment their probes diverge (see test_unaccounted_tokens.py's
    regression on the scheduler side). Prove it two ways: a huge for_7d
    denies even with for_5h=0 (5h healthy on its own), and a huge for_5h
    denies even with for_7d=0 (7d healthy on its own)."""
    # for_7d alone must be able to deny, independent of for_5h.
    resets_7d = _NOW_MS + int(_WEEK_MS * 0.90)  # 10% elapsed -> ramp ~0.10
    windows = _healthy_windows(w5_used=0.0, w5_elapsed_h=4.5)  # 5h healthy on its own
    windows["anthropic:7d"] = _ws("anthropic:7d", used_fraction=0.02, resets_at=resets_7d)
    d = decide(_cfg(), "hunt", windows, UnaccountedTokens(for_5h=0, for_7d=20_000_000))
    assert not d.allow
    assert "anthropic:7d: used" in d.reason

    # for_5h alone must be able to deny, independent of for_7d.
    windows2 = _healthy_windows(w5_used=0.0, w5_elapsed_h=3.0)
    d2 = decide(_cfg(), "hunt", windows2, UnaccountedTokens(for_5h=5_000_000, for_7d=0))
    assert not d2.allow
    assert "5h" in d2.reason


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
    # retry_at: when the 7d ramp would reach used_fraction=0.30 -- 20% of
    # a week from now (started at -10%, needs +30%, currently at -10%+... )
    assert d.retry_at == pytest.approx(_NOW_MS + 0.20 * _WEEK_MS, abs=2000)


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
    assert "ramp" in d.reason


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
    assert "ramp" in d.reason
    # retry_at: ramp reaches 0.60 at 0.45h from now (window started 2.75h
    # ago; 0.60 of the 4.5h harvest ramp, plus the 0.5h headroom, is 3.2h
    # after window start = 0.45h from now).
    assert d.retry_at == pytest.approx(_NOW_MS + 0.45 * 3600 * 1000, abs=2000)

def test_5h_harvest_end_high_usage_allow():
    """4.95h elapsed → ramp ≈ 0.989; usage 0.90 < 0.989 → allow."""
    windows = _healthy_windows(w5_used=0.90, w5_elapsed_h=4.95)
    d = decide(_cfg(), "hunt", windows)
    assert d.allow


def test_5h_exhausted_deny():
    """Exhausted 5h window → deny regardless of timing."""
    windows = _healthy_windows(w5_elapsed_h=4.5)
    resets_at = _NOW_MS + _WEEK_MS // 2
    windows["anthropic:5h"] = _ws(
        "anthropic:5h", used_fraction=1.0, status="exhausted", resets_at=resets_at,
    )
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    # Exhaustion is a hard cap, not a ramp -- resolves exactly at reset.
    assert d.retry_at == resets_at


def test_5h_exhausted_but_stale_still_denies():
    """Regression: an "exhausted" reading older than stale_after_s must NOT
    be treated as "no active window" (opener-safe). resets_at is still in
    the future, so the window is definitely still exhausted -- staleness
    only means nobody has re-probed since, not that it reopened early.
    Production incident: a probe recorded exhausted+ok-fresh, then ~29s
    later (still 4 minutes before resets_at) the SAME reading crossed the
    staleAfterS=1800 age threshold mid-cycle and decide() flipped to allow,
    starting a fix job before the real reset."""
    stale_age = 3600.0  # well above default stale_after_s=1800
    resets_at = _NOW_MS + 4 * 60 * 1000  # reset is still 4 minutes away
    windows = {
        "anthropic:5h": _ws(
            "anthropic:5h", used_fraction=1.0, status="exhausted",
            resets_at=resets_at, age_s=stale_age,
        ),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.10, age_s=stale_age),
    }
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert d.retry_at == resets_at


def test_7d_denial_during_5h_headroom_uses_7d_retry_not_5h_timing():
    """Regression: a 7d-ramp denial that happens to coincide with the 5h
    window's initial headroom period must report a retry_at based on the
    7d ramp, not get confused with 5h headroom timing (the daemon's old
    sleep computation had exactly this bug -- it branched on "are we in
    5h headroom" before even checking whether the denial was a 5h or 7d
    one). decide() checks 7d first and returns immediately on a 7d deny,
    so this is structurally impossible to get wrong now: the 5h headroom
    logic is never reached at all when 7d already denied."""
    resets_5h = _NOW_MS + int(4.75 * 3600 * 1000)  # 15min into a fresh 5h window
    resets_7d = _NOW_MS + int(_WEEK_MS * 0.90)  # 10% elapsed into the 7d window
    windows = {
        "anthropic:5h": _ws("anthropic:5h", used_fraction=0.0, resets_at=resets_5h),
        "anthropic:7d": _ws("anthropic:7d", used_fraction=0.30, resets_at=resets_7d),
    }
    d = decide(_cfg(), "hunt", windows)
    assert not d.allow
    assert d.reason.startswith("anthropic:7d")
    assert d.retry_at == pytest.approx(_NOW_MS + 0.20 * _WEEK_MS, abs=2000)


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


def test_decide_denies_on_unaccounted_alone_through_a_fresh_rollover():
    """End-to-end regression, decide()'s side of the same incident: a 5h
    window that just rolled over (used_fraction=0.0, recorded_at at the
    true cycle boundary -- exactly what read_windows() now produces) must
    still deny once hunter's own unaccounted spend alone crosses the ramp,
    even though the raw probe shows 0% used and looks perfectly healthy.
    Before the fix, this situation never reached decide() at all -- the
    window was dropped upstream and decide() saw "no window data"."""
    elapsed_h = 3.0  # well past headroom -> real ramp
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
    # ~2.9M tokens burned since the rollover (the actual incident's scale) --
    # 2_900_000 / 200_000 * 10% = 1.45 effective usage, far past any ramp.
    d = decide(_cfg(), "hunt", windows, UnaccountedTokens(for_5h=2_900_000))
    assert not d.allow
    assert "5h" in d.reason


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
