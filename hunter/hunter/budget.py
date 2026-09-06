"""Budget policy -- two linear ramps.

7-day ramp:  allowed = elapsed_fraction_of_7d_window.
             Spreads spending evenly across the week.

5-hour ramp: allowed = max(0, (elapsed - HEADROOM) / (5h - HEADROOM)).
             Zero for the first HEADROOM duration (human headroom), then 0→1
             over the remaining time.  Anything unspent at reset is wasted capacity.

No active 5h window → allow (opens one).  The 7d ramp is the outer gate.
Stale 5h data → treated as no active window (opener safe, 7d is the gate).
Missing data entirely → deny.
"""

from __future__ import annotations

import sqlite3
import time

from .types import OMP_AGENT_DB, BudgetDecision, Config, WindowState

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
        if resets_at and resets_at <= now:
            # This window's own cycle has already ended -- e.g. a
            # per-model-class window nobody has probed since hunter's
            # config stopped routing jobs to that model (observed:
            # anthropic:7d:fable, resets_at ~26 days in the past, no
            # fresh row in 26+ days). The recorded used_fraction
            # describes a bygone period, not now -- drop it entirely
            # rather than surface it as if it were current data.
            continue
        out[r["limit_id"]] = WindowState(
            limit_id=r["limit_id"],
            used_fraction=r["used_fraction"],
            status=r["status"],
            resets_at=resets_at,
            recorded_at=r["recorded_at"],
            age_s=(now - r["recorded_at"]) / 1000,
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


def decide(
    cfg: Config,
    kind: str,
    windows: dict[str, WindowState],
    running_jobs_cap: int = 0,
) -> BudgetDecision:
    base = cfg.hunt_cap_tokens if kind == "hunt" else cfg.fix_cap_tokens
    now_ms = time.time() * 1000

    if not windows:
        return BudgetDecision(False, "no window data -- deny until fresh")

    # -- Account for in-flight jobs --------------------------------------------
    # Running jobs reserve budget but OMP hasn't recorded their usage yet.
    # Conservatively estimate: reserve 10% of window capacity per 200k job cap.
    # (Anthropic 5h ~= 2-5M tokens depending on model, so 200k ~= 4-10%)
    inflight_reservation = (running_jobs_cap / 200_000) * 0.10

    # -- 7d linear ramp: spend proportionally to elapsed time ----------------
    # The 7d fraction moves slowly; even somewhat stale data is safe here.

    for lid, w in windows.items():
        if ":7d" not in lid or w.used_fraction is None:
            continue
        if w.resets_at and w.resets_at <= now_ms:
            # This window's own cycle has already ended -- the recorded
            # used_fraction describes a bygone week, not the current one
            # (e.g. a per-model-class window nobody has probed since the
            # config stopped routing jobs to that model; resets_at can be
            # weeks in the past). Using it here would gate live decisions
            # on data for a period that is definitionally over. Skip it,
            # same as "no data for this dimension" -- unlike the missing-
            # resets_at case below, we know for certain this reading is
            # stale, not just imprecise.
            continue
        elapsed_frac = ramp_7d(w.resets_at, now_ms)
        # Reserve budget for running jobs
        effective_used = w.used_fraction + inflight_reservation
        if effective_used >= elapsed_frac:
            return BudgetDecision(
                False,
                f"{lid}: used {w.used_fraction:.2f} + inflight {inflight_reservation:.2f}"
                f" = {effective_used:.2f} >= ramp {elapsed_frac:.2f}",
                retry_at=retry_at_7d(w.resets_at, effective_used),
            )

    # -- 5h ramp (configurable headroom, then linear harvest) ------------------
    #
    # Only trust fresh 5h data (the human could be actively using the window).
    # Stale or missing 5h → treat as no active window → allow (opener).
    # Active window → allowed = max(0, (elapsed - HEADROOM) / (5h - HEADROOM)).

    w5 = windows.get("anthropic:5h")
    if w5 is not None and w5.age_s <= cfg.stale_after_s:
        if w5.status == "exhausted":
            # A hard cap, not a ramp -- resolves exactly at reset, not at
            # some computed ramp-catchup point.
            return BudgetDecision(False, "5h window exhausted", retry_at=w5.resets_at)
        allowed = ramp_5h(w5.resets_at, now_ms)
        if allowed is not None and w5.used_fraction is not None:
            effective_used = w5.used_fraction + inflight_reservation
            if effective_used >= allowed:
                return BudgetDecision(
                    False,
                    f"5h: used {w5.used_fraction:.2f} + inflight {inflight_reservation:.2f}"
                    f" = {effective_used:.2f} >= ramp {allowed:.2f}",
                    retry_at=retry_at_5h(w5.resets_at, effective_used),
                )
    return BudgetDecision(True, "ok", base)
