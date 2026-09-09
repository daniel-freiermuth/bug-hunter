"""Budget policy -- two linear ramps.

7-day ramp:  allowed = elapsed_fraction_of_7d_window.
             Spreads spending evenly across the week.

5-hour ramp: allowed = max(0, (elapsed - HEADROOM) / (5h - HEADROOM)).
             Zero for the first HEADROOM duration (human headroom), then 0→1
             over the remaining time.  Anything unspent at reset is wasted capacity.

Both horizons reduce to one comparison: allowed = ramp(...) > effective_used.
"exhausted" is folded into effective_used (clamped to exactly 1.0, see
_effective_used) rather than special-cased -- since both ramps are
themselves capped at 1.0 (reached only exactly at resets_at), an exhausted
window denies for its whole remaining duration for free, with no
ramp-catchup-before-reset risk.

A window's last known reading is never discarded for being old -- usage
only increases within a window, so it remains a valid floor. Combined
with unaccounted-token tracking (computed by the facade from the
SpendLedger, tracked separately per window), and ramps that keep
climbing toward 1.0 against live wall-clock time regardless of probe
staleness, denial resolves itself once the ramp naturally catches up --
no bypass needed.

No active 5h window (truly nothing ever probed, e.g. a fresh install) →
allow (opens one), gated only by the 7d ramp. A window whose own
resets_at has passed is NOT "no active window" -- read_windows() rolls
anthropic:5h/anthropic:7d forward into their current cycle instead of
dropping them (a genuinely abandoned per-model-class dimension still
gets dropped; see read_windows). Missing data entirely → deny.
"""

from __future__ import annotations

import sqlite3
import time
from dataclasses import dataclass, field
from pathlib import Path

from hunter.types import Config

# ---------------------------------------------------------------------------
# Locally-defined constants and types (decoupled from hunter.types)
# ---------------------------------------------------------------------------

OMP_AGENT_DB = Path.home() / ".omp/agent/agent.db"


@dataclass
class WindowState:
    limit_id: str
    used_fraction: float | None
    status: str | None  # ok | exhausted | ...
    resets_at: int | None  # epoch ms
    recorded_at: int  # epoch ms -- when omp probed it
    age_s: float = field(default=0.0)

    @property
    def stale(self) -> bool:
        return self.age_s > 1800



# =============================================================================
# Configuration: To adjust when hunter can start using the 5h window,
#                change HEADROOM_MS below (e.g., 15min, 1h, 2h).
#                Everything else auto-computes from it.
# =============================================================================
HEADROOM_MS = 30 * 60 * 1000  # Human headroom before harvest window opens

_WEEK_MS = 7 * 24 * 3600 * 1000
_5H_MS = 5 * 3600 * 1000
_RAMP_MS = _5H_MS - HEADROOM_MS  # Harvest window duration (4.5h at 30min headroom)


def read_windows() -> dict[str, WindowState]:
    """Latest usage_history row per anthropic:* limit. Missing DB -> {}."""
    if not OMP_AGENT_DB.exists():
        return {}
    try:
        db = sqlite3.connect(f"file:{OMP_AGENT_DB}?mode=ro", uri=True)
        db.row_factory = sqlite3.Row
        rows = db.execute(
            "SELECT limit_id, used_fraction, status, resets_at,"
            " MAX(recorded_at) AS recorded_at"
            " FROM usage_history WHERE limit_id LIKE 'anthropic:%'"
            " GROUP BY limit_id"
        ).fetchall()
        db.close()
    except sqlite3.Error:
        return {}
    now = time.time() * 1000
    out: dict[str, WindowState] = {}
    for r in rows:
        resets_at = r["resets_at"]
        used_fraction = r["used_fraction"]
        status = r["status"]
        recorded_at = r["recorded_at"]
        if resets_at and resets_at <= now:
            # This window's own cycle has ended -- but that does NOT mean
            # "no active window": 5h/7d windows are back-to-back, so a new
            # cycle is definitely running RIGHT NOW, hunter just hasn't
            # been probed for it yet. Dropping the row entirely (the old
            # behavior) was a real production incident: decide()'s
            # unaccounted_tokens reservation exists exactly to survive a
            # stale-but-still-current probe, but it only ever gets
            # consulted when a WindowState is present to attach it to --
            # once this function dropped the row, decide() saw no
            # anthropic:5h data at all and fell through to its "no active
            # window -> allow" rule, completely bypassing
            # unaccounted_tokens regardless of how large it had grown. A
            # cascade of modernization jobs (which routinely cost 2-5x
            # their nominal cap on a cold-cache first call, see
            # runner.py) burned ~2.9M tokens across a freshly-rolled-over
            # 5h window with zero denials, because nothing was left to
            # deny against.
            #
            # Fix: roll the window forward ourselves instead of dropping
            # it. A brand-new window legitimately starts at 0% (Anthropic's
            # own probe will confirm that once it lands) -- what's NOT
            # legitimate is treating "0% probed" as "0% spent". recorded_at
            # is set to the boundary the new cycle actually started at (not
            # `now`), so _unaccounted_tokens's "finished since the last
            # probe" query correctly sums every job hunter has run since
            # the rollover, not just since this function last happened to
            # run.
            # Scoped to the exact account-wide dimensions decide() actually
            # gates real work on -- NOT per-model-class variants like
            # anthropic:7d:fable, which really can go genuinely abandoned
            # (config stopped routing there) and must stay dropped, both to
            # avoid decide() noise and to keep the Status page from showing
            # a synthesized "fresh" window for a dimension nothing uses.
            period = {"anthropic:5h": _5H_MS, "anthropic:7d": _WEEK_MS}.get(r["limit_id"])
            if period is None:
                continue  # per-model-class or unrecognized -- keep the old, safe drop
            new_resets_at = resets_at
            while new_resets_at <= now:
                recorded_at = new_resets_at
                new_resets_at += period
            resets_at = new_resets_at
            used_fraction = 0.0
            status = "ok"
        out[r["limit_id"]] = WindowState(
            limit_id=r["limit_id"],
            used_fraction=used_fraction,
            status=status,
            resets_at=resets_at,
            recorded_at=recorded_at,
            age_s=(now - recorded_at) / 1000,
        )
    return out


def ramp_7d(resets_at: int | None, now_ms: float) -> float:
    """Fraction of the 7-day linear ramp elapsed so far (0..1) -- this is
    the currently-allowed spend ceiling for a :7d window, the same
    quantity decide() compares used_fraction against. Unknown or already-
    expired resets_at -> 1.0 (assume end-of-window; matches decide()'s
    existing permissive fallback for missing data, and read_windows()
    already drops genuinely-expired rows before this could be called
    with stale data)."""
    if not resets_at or resets_at <= now_ms:
        return 1.0
    started_ms = resets_at - _WEEK_MS
    return min((now_ms - started_ms) / _WEEK_MS, 1.0)


def ramp_5h(resets_at: int | None, now_ms: float) -> float | None:
    """Fraction of the 5h harvest ramp elapsed so far (0..1): 0 for the
    first HEADROOM_MS, then linear to 1.0 at reset. None means there is
    no active window to compute a ramp against (decide() then treats the
    window as an opener -- always-allow)."""
    if not resets_at or resets_at <= now_ms:
        return None
    elapsed_ms = _5H_MS - (resets_at - now_ms)
    return max(0.0, (elapsed_ms - HEADROOM_MS) / _RAMP_MS)


def retry_at_7d(resets_at: int | None, effective_used: float) -> float | None:
    """Epoch ms when the 7d ramp would reach effective_used (assuming no
    further spend) -- the earliest a 7d-ramp denial at this used level
    could resolve. None if resets_at is unknown: no informed estimate is
    possible, callers should fall back to a generic backoff rather than
    a value that looks precise but isn't."""
    if not resets_at:
        return None
    started_ms = resets_at - _WEEK_MS
    return started_ms + effective_used * _WEEK_MS


def retry_at_5h(resets_at: int | None, effective_used: float) -> float | None:
    """Epoch ms when the 5h harvest ramp would reach effective_used
    (assuming no further spend). None if resets_at is unknown."""
    if not resets_at:
        return None
    window_start_ms = resets_at - _5H_MS
    return window_start_ms + HEADROOM_MS + effective_used * _RAMP_MS


def _effective_used(w: WindowState, inflight_reservation: float) -> float:
    """used_fraction, adjusted for spend the probe can't see yet.

    "exhausted" is clamped to exactly 1.0 rather than trusting the raw
    reported fraction: it's Anthropic's hard-stop signal, not a pacing
    number, and its literal value isn't reliable across limit types
    (observed as high as 1.57 for a non-Anthropic limit in this same
    usage_history table). Trusting it verbatim could let a fraction just
    under 1.0 be second-guessed by ramp catch-up before the window is
    actually over, or push a ramp-derived retry_at past the real reset.
    """
    used = 1.0 if w.status == "exhausted" else w.used_fraction
    return used + inflight_reservation

