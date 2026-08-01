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
        out[r["limit_id"]] = WindowState(
            limit_id=r["limit_id"],
            used_fraction=r["used_fraction"],
            status=r["status"],
            resets_at=r["resets_at"],
            recorded_at=r["recorded_at"],
            age_s=(now - r["recorded_at"]) / 1000,
        )
    return out


def decide(cfg: Config, kind: str, windows: dict[str, WindowState]) -> BudgetDecision:
    base = cfg.hunt_cap_tokens if kind == "hunt" else cfg.fix_cap_tokens
    now_ms = time.time() * 1000

    if not windows:
        return BudgetDecision(False, "no window data -- deny until fresh")

    # -- 7d linear ramp: spend proportionally to elapsed time ----------------
    # The 7d fraction moves slowly; even somewhat stale data is safe here.

    for lid, w in windows.items():
        if ":7d" not in lid or w.used_fraction is None:
            continue
        if w.resets_at and w.resets_at > now_ms:
            started_ms = w.resets_at - _WEEK_MS
            elapsed_frac = min((now_ms - started_ms) / _WEEK_MS, 1.0)
        else:
            elapsed_frac = 1.0  # can't compute -> assume end-of-window
        if w.used_fraction >= elapsed_frac:
            return BudgetDecision(
                False,
                f"{lid}: used {w.used_fraction:.2f} >= ramp {elapsed_frac:.2f}"
                f" (elapsed {elapsed_frac:.1%})",
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
            if w5.used_fraction >= allowed:
                return BudgetDecision(
                    False,
                    f"5h: used {w5.used_fraction:.2f} >= ramp {allowed:.2f}"
                    f" (harvest in {(w5.resets_at - _RAMP_MS - now_ms) / 60_000:.0f}min)",
                )
    return BudgetDecision(True, "ok", base)
