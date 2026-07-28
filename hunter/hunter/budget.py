"""Budget policy -- linear ramp over the 7-day window.

Core rule: if fraction p of the 7-day window has elapsed, the scavenger may
have consumed at most p of the total budget.  This spreads spending evenly
across the week.

The 5h window is a short-horizon gate (deny when near-exhausted so the human
isn't locked out mid-afternoon). Stale or missing data -> always deny.
"""

from __future__ import annotations

import sqlite3
import time

from .types import OMP_AGENT_DB, BudgetDecision, Config, WindowState

_CONSERVATIVE_CAP = 120_000
_WEEK_MS = 7 * 24 * 3600 * 1000


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

    # -- 5h gate: don't lock the human out mid-session -----------------------
    w5 = fresh.get("anthropic:5h")
    if w5 is not None and (
        w5.status == "exhausted" or (w5.used_fraction or 0) >= cfg.deny_5h_above
    ):
        return BudgetDecision(False, f"5h window at {w5.used_fraction} ({w5.status})")

    # -- 7d linear ramp: spend proportionally to elapsed time ----------------

    for lid, w in fresh.items():
        if ":7d" not in lid or w.used_fraction is None:
            continue
        if w.resets_at and w.resets_at > now_ms:
            started_ms = w.resets_at - _WEEK_MS
            elapsed_frac = min((now_ms - started_ms) / _WEEK_MS, 1.0)
        else:
            elapsed_frac = 1.0  # can't compute -> assume end-of-window
        allowed = elapsed_frac
        used = w.used_fraction
        if used >= allowed:
            return BudgetDecision(
                False,
                f"{lid}: used {used:.2f} >= ramp {allowed:.2f} (elapsed {elapsed_frac:.1%})",
            )

    # -- allowed: scale cap by remaining ramp headroom -----------------------
    # Find the tightest 7d limit's remaining ramp room
    min_headroom = 1.0
    for lid, w in fresh.items():
        if ":7d" not in lid or w.used_fraction is None or not w.resets_at:
            continue
        started_ms = w.resets_at - _WEEK_MS
        elapsed_frac = min((now_ms - started_ms) / _WEEK_MS, 1.0)
        allowed = elapsed_frac
        headroom = allowed - w.used_fraction
        min_headroom = min(min_headroom, headroom)

    cap = base
    if min_headroom < 0.1:
        cap = base // 2
    reason = f"ok (ramp headroom {min_headroom:.1%})" + (
        ", cap halved" if min_headroom < 0.1 else ""
    )
    return BudgetDecision(True, reason, cap)
