"""OmpScavengeBackend -- the Backend facade for omp + Anthropic scavenging.

Bundles:
- harness: spawn/meter/kill omp workers (harness.py)
- capacity: read Anthropic window state, compute ramps, decide (capacity.py)
- observation: log window probes, record calibration samples, estimate capacity

All Anthropic-specific vocabulary (5h/7d windows, ramp math, probe staleness)
is confined to this package.  Core sees only Outlook / RunResult / HTML.
"""

from __future__ import annotations

import html
import logging
import time
from dataclasses import dataclass
from pathlib import Path

from hunter.backend import (
    Denied,
    Granted,
    JobClass,
    Outlook,
    SpendLedger,
)
from hunter.types import Config, RunResult

from . import capacity
from .harness import run_worker

log = logging.getLogger("hunter.backend")

# Tokens-to-fraction conversion calibration.
# 200k tokens ≈ 10% of a 5h window's capacity.
# The 7d window is ~33.6x larger (168h / 5h).
_TOK_PER_FRAC_5H = 200_000 / 0.10  # 2M tokens = 100% of 5h
_5H_7D_RATIO = capacity._5H_MS / capacity._WEEK_MS  # ≈ 0.0298


def _esc(s: object) -> str:
    """HTML-escape any value for safe interpolation into status fragments."""
    return html.escape(str(s))


# ---------------------------------------------------------------------------
# Calibration durations -- same as the old store._CALIBRATION_DURATIONS_MS
# but owned by the backend now (it's Anthropic window knowledge).
# ---------------------------------------------------------------------------
_CALIBRATION_DURATIONS_MS: dict[str, int] = {
    "5h": capacity._5H_MS,
    "7d": capacity._WEEK_MS,
}


@dataclass
class OmpScavengeBackend:
    """Backend implementation for omp harness + Anthropic scavenging policy.

    Constructed with a Config and a SpendLedger (the Store).
    """

    cfg: Config
    ledger: SpendLedger
    # Last decide()'s per-window reservations — used by status() so the
    # bar's unaccounted segment matches what the decision-maker last saw,
    # rather than recomputing with anticipated=0 and a possibly-fresher probe.
    _last_res: tuple[float, float] = (0.0, 0.0)

    # -- decide --------------------------------------------------------------

    def _unaccounted_fraction(
        self,
        windows: dict[str, capacity.WindowState],
        anticipated: int,
    ) -> tuple[float, float]:
        """Compute unaccounted inflight reservation as fraction, per window.

        Returns (reservation_5h, reservation_7d).
        Each is independently computed against its own window's recorded_at.
        """
        running = self.ledger.running_estimate()

        fallback = min((w.recorded_at for w in windows.values()), default=0)
        w5 = windows.get("anthropic:5h")
        w7 = windows.get("anthropic:7d")
        probe_at_5h = w5.recorded_at if w5 is not None else fallback
        probe_at_7d = w7.recorded_at if w7 is not None else fallback

        base = running + anticipated
        unaccounted_5h = base + self.ledger.finished_since(probe_at_5h)
        unaccounted_7d = base + self.ledger.finished_since(probe_at_7d)

        reservation_5h = (unaccounted_5h / 200_000) * 0.10
        reservation_7d = (unaccounted_7d / 200_000) * 0.10 * _5H_7D_RATIO
        return reservation_5h, reservation_7d

    def _decide_inner(
        self,
        windows: dict[str, capacity.WindowState],
        reservation_5h: float,
        reservation_7d: float,
        *,
        prio: bool,
    ) -> Granted | Denied:
        """One verdict: check 7d ramps then 5h ramp.

        When prio=True, pacing denials become grants (ramp is waived)
        but exhaustion denials stand.
        """
        now_ms = time.time() * 1000

        if not windows:
            return Denied("no window data -- deny until fresh")

        # -- 7d linear ramp ---------------------------------------------------
        for lid, w in windows.items():
            if ":7d" not in lid or (w.status != "exhausted" and w.used_fraction is None):
                continue
            if w.resets_at and w.resets_at <= now_ms:
                continue  # expired cycle -- skip (see capacity.read_windows)
            elapsed_frac = capacity.ramp_7d(w.resets_at, now_ms)
            effective_used = capacity._effective_used(w, reservation_7d)
            if effective_used >= elapsed_frac:
                is_exhausted = w.status == "exhausted"
                retry = (
                    w.resets_at
                    if is_exhausted
                    else capacity.retry_at_7d(w.resets_at, effective_used)
                )
                reason = (
                    f"{lid}: used {effective_used - reservation_7d:.2f}"
                    f" + unaccounted {reservation_7d:.2f}"
                    f" = {effective_used:.2f} >= ramp {elapsed_frac:.2f}"
                )
                if prio and not is_exhausted:
                    # Pacing waived: grant with headroom to hard limit
                    headroom_frac = max(0.0, 1.0 - effective_used)
                    cap = self._frac_to_tokens(headroom_frac, "7d")
                    return Granted(cap_tokens=cap if cap > 0 else None, reason=f"prio override ({reason})")
                return Denied(reason, retry_at=retry)

        # -- 5h ramp ----------------------------------------------------------
        w5 = windows.get("anthropic:5h")
        if w5 is not None and (w5.status == "exhausted" or w5.used_fraction is not None):
            allowed = capacity.ramp_5h(w5.resets_at, now_ms)
            if allowed is not None:
                effective_used = capacity._effective_used(w5, reservation_5h)
                if effective_used >= allowed:
                    is_exhausted = w5.status == "exhausted"
                    retry = (
                        w5.resets_at
                        if is_exhausted
                        else capacity.retry_at_5h(w5.resets_at, effective_used)
                    )
                    reason = (
                        f"5h: used {effective_used - reservation_5h:.2f}"
                        f" + unaccounted {reservation_5h:.2f}"
                        f" = {effective_used:.2f} >= ramp {allowed:.2f}"
                    )
                    if prio and not is_exhausted:
                        headroom_frac = max(0.0, 1.0 - effective_used)
                        cap = self._frac_to_tokens(headroom_frac, "5h")
                        return Granted(cap_tokens=cap if cap > 0 else None, reason=f"prio override ({reason})")
                    return Denied(reason, retry_at=retry)

        # -- All checks passed ------------------------------------------------
        # Compute normal-work headroom: min across all windows' (ramp - used)
        headroom = self._compute_headroom(windows, reservation_5h, reservation_7d, prio=prio)
        return Granted(cap_tokens=headroom, reason="ok")

    def _compute_headroom(
        self,
        windows: dict[str, capacity.WindowState],
        res_5h: float,
        res_7d: float,
        *,
        prio: bool,
    ) -> int | None:
        """Min headroom in tokens across all windows.

        For normal work: headroom = (ramp - effective_used) * capacity.
        For prio work:   headroom = (1.0 - effective_used) * capacity.
        """
        now_ms = time.time() * 1000
        caps: list[int] = []

        for lid, w in windows.items():
            if w.used_fraction is None:
                continue
            if ":7d" in lid:
                ceiling = 1.0 if prio else capacity.ramp_7d(w.resets_at, now_ms)
                eff = capacity._effective_used(w, res_7d)
                frac = max(0.0, ceiling - eff)
                tok = self._frac_to_tokens(frac, "7d")
                caps.append(tok)
            elif ":5h" in lid:
                allowed = capacity.ramp_5h(w.resets_at, now_ms)
                if allowed is not None:
                    ceiling = 1.0 if prio else allowed
                    eff = capacity._effective_used(w, res_5h)
                    frac = max(0.0, ceiling - eff)
                    tok = self._frac_to_tokens(frac, "5h")
                    caps.append(tok)

        return min(caps) if caps else None

    @staticmethod
    def _frac_to_tokens(frac: float, dim: str) -> int:
        """Convert a fraction of window capacity to tokens."""
        if dim == "5h" or ":5h" in dim:
            return int(frac * _TOK_PER_FRAC_5H)
        # 7d window is ~33.6x larger
        return int(frac * _TOK_PER_FRAC_5H / _5H_7D_RATIO)

    def decide(self, *, anticipated_tokens: int) -> Outlook:
        """May background work spend now?  Returns paired verdicts."""
        windows = capacity.read_windows()
        res_5h, res_7d = self._unaccounted_fraction(windows, anticipated_tokens)
        self._last_res = (res_5h, res_7d)

        normal = self._decide_inner(windows, res_5h, res_7d, prio=False)
        # Monotonicity: if normal is granted, prioritized is at least as permissive.
        if isinstance(normal, Granted):
            prioritized = normal
            # But recompute with prio headroom if normal passed
            prio_headroom = self._compute_headroom(windows, res_5h, res_7d, prio=True)
            if prio_headroom is not None and (
                normal.cap_tokens is None or prio_headroom > normal.cap_tokens
            ):
                prioritized = Granted(cap_tokens=prio_headroom, reason="ok")
        else:
            prioritized = self._decide_inner(windows, res_5h, res_7d, prio=True)

        return Outlook(normal=normal, prioritized=prioritized)

    # -- run -----------------------------------------------------------------

    def run(
        self,
        cwd: Path,
        prompt: str,
        *,
        cap_tokens: int,
        max_wall_s: int,
        job_class: JobClass,
    ) -> RunResult:
        """Execute a worker job, internalizing the usage-delta sandwich."""
        pre = self._usage_snapshot()
        model = self.cfg.model_for(job_class.value)
        rr = run_worker(
            self.cfg,
            cwd,
            prompt,
            cap_tokens,
            max_wall_s,
            model=model,
        )
        post = self._usage_snapshot()
        rr.usage_delta = (
            (post - pre) if pre is not None and post is not None else None
        )
        return rr

    def _usage_snapshot(self) -> float | None:
        """Max used_fraction across 7d windows, or None."""
        windows = capacity.read_windows()
        fracs = [
            w.used_fraction
            for k, w in windows.items()
            if ":7d" in k and w.used_fraction is not None
        ]
        return max(fracs) if fracs else None

    # -- keep_fresh ----------------------------------------------------------

    def keep_fresh(self) -> bool:
        """Refresh stale probes and log observations + calibration."""
        windows = capacity.read_windows()

        # Log observations and record calibration samples
        self._observe(windows)

        # Refresh stale probe
        w5 = windows.get("anthropic:5h")
        if w5 is not None and w5.age_s <= self.cfg.stale_after_s:
            return False

        from hunter.util import run_cmd

        run_cmd(
            [self.cfg.omp_bin, "usage", "invalidate", "--provider", "anthropic"],
            timeout=15,
        )
        rc, _out = run_cmd(
            [self.cfg.omp_bin, "usage", "--provider", "anthropic"],
            timeout=30,
        )
        return rc == 0

    def _observe(self, windows: dict[str, capacity.WindowState]) -> None:
        """Log window observations and record calibration samples.

        Moved from store.log_window -- the calibration logic (horizon
        matching, fresh-probe detection) is Anthropic-window knowledge
        that belongs here.
        """
        now = int(time.time() * 1000)
        for w in windows.values():
            # Calibration: if this is a fresh probe (fraction moved),
            # record a sample correlating hunter's spend with fraction delta
            horizon = next(
                (h for h in _CALIBRATION_DURATIONS_MS if f":{h}" in w.limit_id),
                None,
            )
            if (
                horizon
                and w.resets_at
                and w.used_fraction is not None
            ):
                prev = self.ledger.last_window_observation(w.limit_id, w.resets_at)
                if (
                    prev is not None
                    and w.used_fraction > prev[1]
                    and (now - prev[0]) <= _CALIBRATION_DURATIONS_MS[horizon]
                ):
                    tok = self.ledger.finished_between(prev[0], now)
                    if tok > 0:
                        self.ledger.record_calibration_sample(
                            w.limit_id,
                            w.resets_at,
                            w.used_fraction - prev[1],
                            tok,
                        )

            # Always log the observation
            self.ledger.log_window_observation(
                w.limit_id, w.used_fraction, w.status, w.resets_at, w.age_s,
            )

    # -- status --------------------------------------------------------------

    def status(self) -> str:
        """Render the backend's status panel as an HTML fragment."""
        windows = capacity.read_windows()
        if not windows:
            return '<div class="scv-note">No window data available</div>'

        now_ms = time.time() * 1000
        # Use cached reservations from last decide() so the bar matches
        # the deny reason the user sees, rather than recomputing with
        # anticipated=0 and a possibly-fresher probe.
        res_5h, res_7d = self._last_res
        parts: list[str] = []

        for lid, w in sorted(windows.items(), key=lambda kv: kv[0]):
            label = lid.replace("anthropic:", "") + " window"
            used_pct = (
                f"{w.used_fraction * 100:.0f}%" if w.used_fraction is not None else "?"
            )

            # Unaccounted fraction for this dimension
            if ":5h" in lid:
                unacct = res_5h
                ramp = capacity.ramp_5h(w.resets_at, now_ms)
                # Window elapsed fraction (independent of ramp/headroom)
                if w.resets_at and w.resets_at > now_ms:
                    elapsed_frac = (capacity._5H_MS - (w.resets_at - now_ms)) / capacity._5H_MS
                else:
                    elapsed_frac = None
            elif ":7d" in lid:
                unacct = res_7d
                ramp = capacity.ramp_7d(w.resets_at, now_ms)
                elapsed_frac = None  # 7d doesn't have headroom
            else:
                unacct = 0.0
                ramp = None
                elapsed_frac = None

            fill_pct = min(100, round((w.used_fraction or 0) * 100))
            soft_pct = min(100 - fill_pct, max(0, round(unacct * 100)))
            ramp_pct = min(100, round(ramp * 100)) if ramp is not None else None

            # Available headroom
            avail_frac = max(0.0, (ramp if ramp is not None else 1.0) - (w.used_fraction or 0) - unacct)
            avail_pct = f"{avail_frac * 100:.0f}%"
            cap = self.ledger.estimate_capacity(lid)
            avail_tok = avail_frac * cap if cap is not None else None
            avail_str = (
                f" \u00b7 {avail_pct} avail (~{self._fmt_tokens(avail_tok)} tok)"
                if avail_tok is not None
                else f" \u00b7 {avail_pct} avail"
            ) if w.used_fraction is not None else ""

            # Determine tone
            is_stale = w.age_s > 1800
            is_exhausted = w.status == "exhausted" or (w.used_fraction is not None and w.used_fraction >= 1.0)
            tone = "stale" if is_stale else ("bad" if is_exhausted else "ok")

            # Probe age
            probe_age = f"{w.age_s / 60:.0f}m ago"

            # Reset countdown + absolute time
            if w.resets_at:
                remain_s = (w.resets_at - now_ms) / 1000
                reset_abs = time.strftime("%I:%M %p", time.localtime(w.resets_at / 1000)).lstrip("0")
                if remain_s > 0:
                    h, m = int(remain_s // 3600), int((remain_s % 3600) // 60)
                    countdown = f"{h}h{m:02d}m" if h else f"{m}m"
                    reset_str = f"resets {reset_abs} ({countdown})"
                else:
                    reset_str = "resetting"
            else:
                reset_str = "reset unknown"

            # Headroom indicator for 5h window
            headroom_str = ""
            if elapsed_frac is not None and ramp is not None and ramp == 0.0 and elapsed_frac > 0:
                headroom_remain_ms = capacity.HEADROOM_MS - (elapsed_frac * capacity._5H_MS)
                if headroom_remain_ms > 0:
                    hm = int(headroom_remain_ms / 60_000)
                    headroom_str = f" \u00b7 headroom {hm}m"

            # Unaccounted display
            unacct_str = f" +{unacct * 100:.0f}% in flight" if unacct > 0.005 else ""

            # Build the bar -- hide ramp marker when at 0% (headroom period:
            # marker would sit behind the fill bar, invisible and confusing)
            marker = (
                f'<i class="scv-ramp" style="left:{ramp_pct}%"></i>'
                if ramp_pct is not None and ramp_pct > 0
                else ""
            )
            parts.append(
                f'<div class="scv-win">'
                f'<div class="scv-lab"><b>{_esc(label)}</b>'
                f"<span>{_esc(used_pct)} used{_esc(unacct_str)}{_esc(avail_str)}"
                f'{" \u26a0\ufe0fstale" if is_stale else ""}</span></div>'
                f'<div class="scv-bar">'
                f'<i class="scv-fill scv-{tone}" style="width:{fill_pct}%"></i>'
                f'<i class="scv-soft" style="width:{soft_pct}%"></i>'
                f"{marker}</div>"
                f'<div class="scv-sub">{_esc(reset_str)}{_esc(headroom_str)} \u00b7 probed {_esc(probe_age)}</div>'
                f"</div>"
            )

        return "\n".join(parts)

    @staticmethod
    def _fmt_tokens(n: float) -> str:
        """Format token count with K/M suffix."""
        if n >= 1_000_000:
            return f"{n / 1_000_000:.1f}M"
        if n >= 1_000:
            return f"{n / 1_000:.0f}k"
        return str(int(n))
