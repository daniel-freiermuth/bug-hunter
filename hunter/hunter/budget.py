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
        if w.resets_at:
            started_ms = w.resets_at - _WEEK_MS
            elapsed_frac = min((now_ms - started_ms) / _WEEK_MS, 1.0)
        else:
            elapsed_frac = 1.0  # can't compute -> assume end-of-window
        # Reserve budget for running jobs
        effective_used = w.used_fraction + inflight_reservation
        if effective_used >= elapsed_frac:
            return BudgetDecision(
                False,
                f"{lid}: used {w.used_fraction:.2f} + inflight {inflight_reservation:.2f}"
                f" = {effective_used:.2f} >= ramp {elapsed_frac:.2f}",
            )

    # -- 5h ramp (configurable headroom, then linear harvest) ------------------
    #
    # Only trust fresh 5h data (the human could be actively using the window).
    # Stale or missing 5h → treat as no active window → allow (opener).
    # Active window → allowed = max(0, (elapsed - HEADROOM) / (5h - HEADROOM)).

    w5 = windows.get("anthropic:5h")
    if w5 is not None and w5.age_s <= cfg.stale_after_s:
        if w5.status == "exhausted":
            return BudgetDecision(False, "5h window exhausted")
        if w5.resets_at and w5.resets_at > now_ms and w5.used_fraction is not None:
            elapsed_ms = _5H_MS - (w5.resets_at - now_ms)
            allowed = max(0.0, (elapsed_ms - HEADROOM_MS) / _RAMP_MS)
            effective_used = w5.used_fraction + inflight_reservation
            if effective_used >= allowed:
                return BudgetDecision(
                    False,
                    f"5h: used {w5.used_fraction:.2f} + inflight {inflight_reservation:.2f}"
                    f" = {effective_used:.2f} >= ramp {allowed:.2f}",
                )
    return BudgetDecision(True, "ok", base)
