"""Backend protocol -- the boundary between hunter's scheduler/server and
the harness + provider-accounting + budget-policy bundle.

A backend answers three questions:
  1. May background work spend now, and up to how much?  (decide)
  2. Run this job.                                       (run)
  3. What's your status?                                 (status / keep_fresh)

Decisions cross this boundary as *data* (Outlook); diagnostics cross as
*presentation* (an HTML fragment the backend fully owns).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import Protocol, runtime_checkable

from .types import RunResult


# ---------------------------------------------------------------------------
# Budget-decision types
# ---------------------------------------------------------------------------

class JobClass(StrEnum):
    """The two budget/model classes the scheduler collapses all job kinds
    into at the backend boundary.  Job kinds (hunt, fix, engage, harvest,
    recheck, test_gap, ...) are core vocabulary; only the two *classes*
    cross into the backend -- one for exploratory/analysis work, one for
    patch/fix work."""
    HUNT = "hunt"
    FIX = "fix"


@dataclass(frozen=True)
class Granted:
    """Backend allows spending.

    cap_tokens: the backend's own spend ceiling for this verdict -- the
    maximum it considers safe given current accounting state.  None means
    the backend imposes no ceiling (e.g. an unlimited local model).
    Core still applies min(granted.cap_tokens, cfg.*_cap_tokens) --
    workload-policy caps are core's business, not the backend's.

    reason: prose, flows into job notes and the UI.  Never machine-matched.
    """
    cap_tokens: int | None = None
    reason: str = "ok"


@dataclass(frozen=True)
class Denied:
    """Backend denies spending.

    reason: prose, for notes/events/UI.  Never machine-matched.
    retry_at: epoch ms -- best-known time this denial would resolve.
    Drives _compute_sleep_s in the daemon loop.  None = no informed estimate.
    """
    reason: str
    retry_at: float | None = None


Verdict = Granted | Denied


@dataclass(frozen=True)
class Outlook:
    """Paired verdicts: one for normal work, one for prioritized (override).

    INVARIANT: prioritized is at least as permissive as normal --
    if normal is Granted, prioritized must also be Granted.

    The scheduler indexes with:
        verdict = outlook.prioritized if override else outlook.normal
    and never tells the backend which it wanted.
    """
    normal: Verdict
    prioritized: Verdict


# ---------------------------------------------------------------------------
# Spend ledger -- narrow port from store into the backend
# ---------------------------------------------------------------------------

@runtime_checkable
class SpendLedger(Protocol):
    """What a backend may know about hunter's own job spend and
    observation history.

    Implemented by Store.  The backend uses this to:
    - compute spend the provider's probe hasn't seen yet (unaccounted)
    - log window observations and calibration samples
    - estimate window capacity from accumulated samples
    """

    def running_estimate(self) -> int:
        """SUM(cap_tokens) of jobs currently in state='running'."""
        ...

    def finished_since(self, ts_ms: int) -> int:
        """SUM(tokens_new) of jobs that finished after ts_ms."""
        ...

    def finished_between(self, start_ms: int, end_ms: int) -> int:
        """SUM(tokens_new) of jobs that finished in (start_ms, end_ms]."""
        ...

    def log_window_observation(
        self,
        limit_id: str,
        used_fraction: float | None,
        status: str | None,
        resets_at: int | None,
        age_s: float,
    ) -> None:
        """Record a single window observation to window_log."""
        ...

    def last_window_observation(
        self, limit_id: str, resets_at: int
    ) -> tuple[int, float] | None:
        """Most recent (observed_at, used_fraction) for this limit_id
        and resets_at cycle.  None if no prior observation."""
        ...

    def record_calibration_sample(
        self,
        limit_id: str,
        window_resets_at: int,
        used_fraction_delta: float,
        hunter_tokens: int,
    ) -> None:
        """Record a calibration sample correlating token spend with
        fraction movement."""
        ...

    def estimate_capacity(
        self, limit_id: str, min_delta: float = 0.02, sample_limit: int = 200
    ) -> float | None:
        """Empirical p75 estimate of this window's total token capacity."""
        ...


# ---------------------------------------------------------------------------
# Backend protocol
# ---------------------------------------------------------------------------

@runtime_checkable
class Backend(Protocol):
    """The backend facade -- harness + accounting + budget policy."""

    def decide(self, *, anticipated_tokens: int) -> Outlook:
        """May background work spend now?

        anticipated_tokens: the caller's pre-reservation for the job being
        decided on (see scheduler.anticipated_tokens -- a realistic estimate
        of this job's cost based on historical data and cache-warmth).

        Returns an Outlook with verdicts for both normal and prioritized work.
        Each Granted verdict carries the backend's own cap_tokens ceiling.
        """
        ...

    def run(
        self,
        cwd: Path,
        prompt: str,
        *,
        cap_tokens: int,
        max_wall_s: int,
        job_class: JobClass,
    ) -> RunResult:
        """Execute a worker job.

        cap_tokens: the effective cap -- already min'd by the caller from
        the Granted verdict's cap and core config.
        job_class: drives model selection (the backend owns model config).
        """
        ...

    def keep_fresh(self) -> bool:
        """Maintenance entrypoint: refresh stale accounting data, log
        observations, record calibration samples.

        Called from the server's dedicated prober thread on a fixed tick,
        decoupled from job-dispatch cadence.

        Returns True if a refresh was performed.
        """
        ...

    def status(self) -> str:
        """Render the backend's status panel as an HTML fragment.

        The UI injects this via innerHTML on the existing 5s poll cycle.
        The backend has full freedom over content and styling.

        Disciplines:
        - All interpolated data must be HTML-escaped (backend's job).
        - Styles use a backend-specific class prefix to avoid collisions.
        - Fragment must be stateless-render-safe (no interactive DOM state;
          rebuilt every poll tick).
        """
        ...
