"""Budget policy -- two linear ramps.

7-day ramp:  allowed = elapsed_fraction_of_7d_window.
             Spreads spending evenly across the week.

5-hour ramp: allowed = max(0, (elapsed - 4h) / 1h).
             Zero for the first 4 hours (human headroom), then 0→1 over the
             last hour.  Anything unspent at reset is wasted capacity.

No active 5h window → allow (opens one).  The 7d ramp is the outer gate.
Stale or missing data → deny.
"""

from __future__ import annotations

import sqlite3
import time

from .types import OMP_AGENT_DB, BudgetDecision, Config, WindowState

_WEEK_MS = 7 * 24 * 3600 * 1000
_5H_MS = 5 * 3600 * 1000
_4H_MS = 4 * 3600 * 1000
_1H_MS = 1 * 3600 * 1000


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

    fresh = {k: w for k, w in windows.items() if w.age_s <= cfg.stale_after_s}
    if not fresh:
        return BudgetDecision(False, "window data stale or missing -- deny until fresh")

    # -- 7d linear ramp: spend proportionally to elapsed time ----------------

    for lid, w in fresh.items():
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

    # -- 5h last-hour ramp ---------------------------------------------------
    #
    # No active 5h window → allow (this job opens one).
    # Active window → allowed = max(0, (elapsed - 4h) / 1h).

    w5 = fresh.get("anthropic:5h")
    if w5 is not None:
        if w5.status == "exhausted":
            return BudgetDecision(False, f"5h window exhausted")
        if w5.resets_at and w5.resets_at > now_ms and w5.used_fraction is not None:
            elapsed_ms = _5H_MS - (w5.resets_at - now_ms)
            allowed = max(0.0, (elapsed_ms - _4H_MS) / _1H_MS)
            if w5.used_fraction >= allowed:
                return BudgetDecision(
                    False,
                    f"5h: used {w5.used_fraction:.2f} >= ramp {allowed:.2f}"
                    f" (harvest in {(w5.resets_at - _1H_MS - now_ms) / 60_000:.0f}min)",
                )

    return BudgetDecision(True, "ok", base)
