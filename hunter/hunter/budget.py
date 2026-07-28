"""Budget policy — two horizons (exp2): 5h window is the scheduling grain,
the 7-day caps are the true budget. Reads omp's local usage mirror; treats
stale rows as unknown and falls back to conservative caps.
"""
from __future__ import annotations

import sqlite3
import time

from .types import OMP_AGENT_DB, BudgetDecision, Config, WindowState

_CONSERVATIVE_CAP = 120_000


def read_windows() -> dict[str, WindowState]:
    """Latest usage_history row per anthropic:* limit. Missing DB -> {}."""
    if not OMP_AGENT_DB.exists():
        return {}
    try:
        db = sqlite3.connect(f"file:{OMP_AGENT_DB}?mode=ro", uri=True)
        db.row_factory = sqlite3.Row
        rows = db.execute(
            "SELECT limit_id, used_fraction, status, resets_at, MAX(recorded_at) AS recorded_at"
            " FROM usage_history WHERE limit_id LIKE 'anthropic:%' GROUP BY limit_id"
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

    if not windows:
        return BudgetDecision(True, "no window data — conservative cap",
                              min(base, _CONSERVATIVE_CAP))

    fresh = {k: w for k, w in windows.items() if w.age_s <= cfg.stale_after_s}

    w5 = fresh.get("anthropic:5h")
    if w5 is not None:
        if w5.status == "exhausted" or (w5.used_fraction or 0) >= cfg.deny_5h_above:
            return BudgetDecision(False, f"5h window at {w5.used_fraction} ({w5.status})")

    scavenge_ceiling = 1.0 - cfg.interactive_reserve_7d
    weekly_pressure = 0.0
    for lid, w in fresh.items():
        if ":7d" in lid and w.used_fraction is not None:
            if w.used_fraction >= scavenge_ceiling:
                return BudgetDecision(False, f"{lid} at {w.used_fraction} ≥ weekly reserve line {scavenge_ceiling:.2f}")
            weekly_pressure = max(weekly_pressure, w.used_fraction)

    cap = base
    reason = f"ok (weekly pressure {weekly_pressure:.2f})"
    if weekly_pressure >= 0.8 * scavenge_ceiling:
        cap = base // 2
        reason = f"weekly pressure {weekly_pressure:.2f} near reserve — cap halved"

    if not fresh:
        return BudgetDecision(False, "window data stale or missing — deny until fresh")

    return BudgetDecision(True, reason, cap)
