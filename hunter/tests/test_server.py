"""Tests for hunter.server._describe_cycle, _compute_sleep_s,
_activity_status, and _validate_summary.

_describe_cycle classifies run_cycle's return shape into (state, detail)
for the Status page's "last log" line. _compute_sleep_s decides how long
the daemon loop sleeps after a cycle attempt. _activity_status is the
single canonical answer to "what is hunter doing right now" -- every
rendering surface (the activity panel, the manual-run button) must
derive its text from this and nothing else; see its docstring for the
four real incidents in one session that came from computing it more
than once, independently, in different places. _validate_summary is the
runtime (pydantic) re-check of the whole /api/summary payload against
SummaryDict, right before serialization -- the network-boundary half of
the guarantee TypedDicts + mypy provide on the Python-internal half
(see SummaryDict's docstring). All four are extracted from the daemon
loop / summary endpoint specifically so this logic is provable by test
rather than only observable by running the real infinite loop or
clicking around the UI.
"""

from __future__ import annotations

import threading
import time
from pathlib import Path

import pytest
from pydantic import ValidationError

from hunter import budget as budget_module
from hunter import scheduler as scheduler_module
from hunter import server
from hunter.server import (
    PR_SYNC_INTERVAL_S,
    USAGE_PROBE_TICK_S,
    _activity_status,
    _compute_sleep_s,
    _describe_cycle,
    _usage_prober_loop,
    _validate_summary,
)
from hunter.store import Store
from hunter.types import Config


class TestDescribeCycle:
    def test_error(self) -> None:
        state, detail = _describe_cycle({"error": "boom"})
        assert state == "error"
        assert "boom" in detail

    def test_idle_no_work(self) -> None:
        state, detail = _describe_cycle({"idle": "no queued findings, no enabled repos"})
        assert state == "idle"
        assert "no enabled repos" in detail

    def test_skipped(self) -> None:
        state, detail = _describe_cycle({"skipped": "finding #1 is 'new', not queued"})
        assert state == "idle"
        assert "not queued" in detail

    def test_denied(self) -> None:
        """"last:" makes clear this describes the last completed cycle,
        not the current instant -- it can sit unchanged for the whole
        sleep interval while a fresh preview elsewhere already differs."""
        state, detail = _describe_cycle({"denied": "5h window exhausted", "job": 3})
        assert state == "denied"
        assert detail == "last: 5h window exhausted"

    def test_finding_outcome(self) -> None:
        state, detail = _describe_cycle(
            {"kind": "fix", "finding": 42, "job": 7, "state": "done", "outcome": "pr_open"}
        )
        assert state == "idle"
        assert "fix" in detail
        assert "#42" in detail
        assert "pr_open" in detail

    def test_repo_outcome(self) -> None:
        state, detail = _describe_cycle(
            {"kind": "hunt", "repo": "myrepo", "job": 9, "state": "done"}
        )
        assert state == "idle"
        assert "hunt" in detail
        assert "myrepo" in detail

    def test_killed_without_outcome_shows_terminal_state(self) -> None:
        state, detail = _describe_cycle(
            {"kind": "hunt", "repo": "myrepo", "job": 9, "state": "killed"}
        )
        assert state == "idle"
        assert "killed" in detail

    def test_unrecognized_shape_falls_back_gracefully(self) -> None:
        state, detail = _describe_cycle({"something": "unexpected"})
        assert state == "idle"
        assert detail


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")




@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)




class TestComputeSleepS:
    def test_error_backs_off_5min(self, store: Store) -> None:
        assert _compute_sleep_s(store, {"error": "boom"}) == 5 * 60

    def test_queued_fixes_drain_immediately(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(
            rid,
            {
                "fingerprint": "fp", "file": "f.py", "symbol": "fn", "line": 1,
                "bug_class": "logic", "severity": "high", "confidence": 0.9,
                "summary": "s", "detail": "d", "evidence_plan": "p", "introduced_by": "x",
            },
        )
        store.set_status(fid, "queued")
        assert _compute_sleep_s(store, {"state": "done"}) == 0

    def test_new_findings_keep_momentum(self, store: Store) -> None:
        summary = {"state": "done", "ingest": {"inserted": 3}}
        assert _compute_sleep_s(store, summary) == 5

    def test_repos_enabled_no_work_periodic_check(self, store: Store) -> None:
        store.add_repo("r", "https://r", "/r")
        assert _compute_sleep_s(store, {"state": "done"}) == 60

    def test_truly_idle_no_repos_backs_off(self, store: Store) -> None:
        assert _compute_sleep_s(store, {"state": "done"}) == 15 * 60

    def test_denied_with_retry_at_derives_sleep(self, store: Store) -> None:
        import time

        retry_at = (time.time() + 120) * 1000  # 2 minutes from now
        sleep_s = _compute_sleep_s(store, {"denied": "x", "retry_at": retry_at})
        assert sleep_s == pytest.approx(150, abs=2)  # 120 + 30 buffer

    def test_denied_without_retry_at_generic_backoff(self, store: Store) -> None:
        assert _compute_sleep_s(store, {"denied": "x", "retry_at": None}) == 30 * 60

    def test_unrecognized_shape_uses_default(self, store: Store) -> None:
        assert _compute_sleep_s(store, {"idle": "nothing to do"}) == 15 * 60

    def test_sync_caps_a_long_denial_backoff(self, store: Store) -> None:
        """The whole point: a multi-hour 7d-ramp denial must not also
        delay noticing PR feedback, which costs nothing to check."""
        summary = {"denied": "x", "retry_at": None, "sync": {"synced": 1}}
        assert _compute_sleep_s(store, summary) == PR_SYNC_INTERVAL_S

    def test_sync_does_not_lengthen_an_already_short_sleep(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(
            rid,
            {
                "fingerprint": "fp2", "file": "f.py", "symbol": "fn", "line": 1,
                "bug_class": "logic", "severity": "high", "confidence": 0.9,
                "summary": "s", "detail": "d", "evidence_plan": "p", "introduced_by": "x",
            },
        )
        store.set_status(fid, "queued")
        summary = {"state": "done", "sync": {"synced": 1}}
        assert _compute_sleep_s(store, summary) == 0


# ---------------------------------------------------------------------------
# _activity_status: the single canonical "what's happening right now"
# ---------------------------------------------------------------------------


_JOB = {"id": 1, "kind": "hunt", "repo_name": "r", "finding_id": None}
_ALLOWED = {
    "kind": "hunt", "id": 12, "label": "r", "is_finding": False,
    "budget_state": "allowed", "budget_reason": "ok", "budget_retry_at": None,
}
_EXEMPT = {**_ALLOWED, "budget_state": "exempt", "budget_reason": "override: x"}
_DENIED = {
    "kind": "hunt", "id": 12, "label": "r", "is_finding": False,
    "budget_state": "denied", "budget_reason": "5h: used 0.5 >= ramp 0.4",
    "budget_retry_at": 999,
}
_ERROR_STATE = {
    "id": 1, "state": "error", "detail": "daemon loop crashed: boom",
    "next_wake_at": None, "updated_at": 0,
}
_IDLE_STATE = {
    "id": 1, "state": "idle", "detail": "last: nothing to do",
    "next_wake_at": None, "updated_at": 0,
}


class TestActivityStatus:
    def test_current_job_wins_over_everything(self) -> None:
        """Regression (incident 2): current_job must be checked first --
        a job that's actually running is ground truth, not a candidate
        to weigh against cycle_running/error/next_candidate."""
        result = _activity_status(_JOB, True, _DENIED, _ERROR_STATE)
        assert result["kind"] == "running"
        assert result["job"] == _JOB

    def test_cycle_running_wins_over_stale_ready_preview(self) -> None:
        """Regression (incident 4): a real cycle in flight (lock held)
        outranks an independently-computed next_candidate preview that
        doesn't know that cycle is about to supersede it."""
        result = _activity_status(None, True, _ALLOWED, _IDLE_STATE)
        assert result["kind"] == "working"

    def test_cycle_running_wins_over_stale_paused_preview(self) -> None:
        result = _activity_status(None, True, _DENIED, _IDLE_STATE)
        assert result["kind"] == "working"

    def test_cycle_running_wins_over_error(self) -> None:
        result = _activity_status(None, True, None, _ERROR_STATE)
        assert result["kind"] == "working"

    def test_error_wins_over_candidate(self) -> None:
        result = _activity_status(None, False, _ALLOWED, _ERROR_STATE)
        assert result["kind"] == "error"
        assert result["detail"] == _ERROR_STATE["detail"]

    def test_denied_candidate_is_paused(self) -> None:
        result = _activity_status(None, False, _DENIED, _IDLE_STATE)
        assert result["kind"] == "paused"
        assert result["candidate"] == _DENIED

    def test_allowed_candidate_is_ready_not_idle(self) -> None:
        """Regression (incident 3): an allowed candidate must never read
        as "idle" -- nothing is blocking it."""
        result = _activity_status(None, False, _ALLOWED, _IDLE_STATE)
        assert result["kind"] == "ready"
        assert result["candidate"] == _ALLOWED

    def test_exempt_candidate_is_also_ready(self) -> None:
        result = _activity_status(None, False, _EXEMPT, _IDLE_STATE)
        assert result["kind"] == "ready"

    def test_no_candidate_but_scheduler_state_is_idle(self) -> None:
        result = _activity_status(None, False, None, _IDLE_STATE)
        assert result["kind"] == "idle"

    def test_nothing_at_all_is_warming_up(self) -> None:
        result = _activity_status(None, False, None, None)
        assert result["kind"] == "warming_up"


# ---------------------------------------------------------------------------
# _validate_summary: the runtime re-check at the network boundary
# ---------------------------------------------------------------------------


def _valid_summary() -> dict:
    """A minimal, fully-valid SummaryDict-shaped payload. Every test
    below starts from this and breaks exactly one thing, so a failure
    always isolates to the field under test."""
    return {
        "windows": {
            "anthropic:5h": {
                "used_fraction": 0.3, "status": "ok", "resets_at": 999,
                "age_s": 1.0, "stale": False, "ramp": 0.4, "available_tokens": None,
            },
        },
        "counts": {"new": 1},
        "type_counts": {"bug": 1},
        "repos": [
            {
                "id": 1, "name": "r", "url": "https://r", "path": "/r", "forge": "github",
                "default_branch": "main", "last_hunt_sha": None, "last_hunt_at": None,
                "enabled": 1, "added_at": 0,
            },
        ],
        "last_cycle": None,
        "cycle_running": False,
        "current_job": None,
        "next_candidate": None,
        "scheduler_state": None,
        "activity_status": {"kind": "idle"},
    }


class TestValidateSummary:
    def test_valid_payload_passes_through_unchanged(self) -> None:
        payload = _valid_summary()
        assert _validate_summary(payload) == payload

    def test_wrong_type_is_rejected(self) -> None:
        """The exact class of bug this closes: Row = dict[str, Any] would
        have accepted this silently and shipped it over the wire."""
        payload = _valid_summary()
        payload["cycle_running"] = "not-a-bool"
        with pytest.raises(ValidationError, match="cycle_running"):
            _validate_summary(payload)

    def test_missing_required_key_is_rejected(self) -> None:
        payload = _valid_summary()
        del payload["counts"]
        with pytest.raises(ValidationError, match="counts"):
            _validate_summary(payload)

    def test_nested_shape_violation_is_rejected(self) -> None:
        """A malformed repo entry -- e.g. from a future code path that
        builds the repos list differently -- is caught even though it's
        nested three levels deep in the payload."""
        payload = _valid_summary()
        del payload["repos"][0]["url"]
        with pytest.raises(ValidationError, match="url"):
            _validate_summary(payload)

    def test_unknown_activity_status_kind_is_rejected(self) -> None:
        """The discriminated union's Literal tags are enforced here too --
        a typo'd or since-removed kind fails loudly instead of shipping
        a shape the frontend's exhaustiveness check has never seen."""
        payload = _valid_summary()
        payload["activity_status"] = {"kind": "bogus"}
        with pytest.raises(ValidationError):
            _validate_summary(payload)


def _cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "h.db")


class TestUsageProberLoop:
    """_usage_prober_loop is the fix for refresh_stale_probe potentially
    starving for up to 60min if it were folded into the job-dispatch
    loop's own variable backoff -- it must be a genuinely independent
    thread with its own short, fixed tick, unaffected by whatever the
    job-dispatch loop is doing (idle, denied, mid-job)."""

    def test_runs_immediately_without_waiting_a_full_tick(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls: list[int] = []
        monkeypatch.setattr(budget_module, "read_windows", dict)
        monkeypatch.setattr(
            scheduler_module, "refresh_stale_probe", lambda cfg, w: calls.append(1) or False
        )
        # A tick this long would never fire a second time within the test's
        # lifetime -- isolates "ran immediately on start" from "also ticks".
        monkeypatch.setattr(server, "USAGE_PROBE_TICK_S", 3600.0)
        stop = threading.Event()
        t = threading.Thread(target=_usage_prober_loop, args=(_cfg(tmp_path), stop), daemon=True)
        t.start()
        for _ in range(100):
            if calls:
                break
            time.sleep(0.02)
        stop.set()
        t.join(timeout=2)
        assert calls == [1]

    def test_ticks_again_after_the_configured_interval(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls: list[int] = []
        monkeypatch.setattr(budget_module, "read_windows", dict)
        monkeypatch.setattr(
            scheduler_module, "refresh_stale_probe", lambda cfg, w: calls.append(1) or False
        )
        monkeypatch.setattr(server, "USAGE_PROBE_TICK_S", 0.02)
        stop = threading.Event()
        t = threading.Thread(target=_usage_prober_loop, args=(_cfg(tmp_path), stop), daemon=True)
        t.start()
        for _ in range(200):
            if len(calls) >= 3:
                break
            time.sleep(0.01)
        stop.set()
        t.join(timeout=2)
        assert len(calls) >= 3

    def test_stops_promptly_when_the_stop_event_is_set(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(budget_module, "read_windows", dict)
        monkeypatch.setattr(scheduler_module, "refresh_stale_probe", lambda cfg, w: False)
        monkeypatch.setattr(server, "USAGE_PROBE_TICK_S", 5.0)
        stop = threading.Event()
        t = threading.Thread(target=_usage_prober_loop, args=(_cfg(tmp_path), stop), daemon=True)
        t.start()
        time.sleep(0.05)  # let it complete its immediate first tick
        stop.set()
        t.join(timeout=2)
        assert not t.is_alive()

    def test_a_failed_tick_does_not_crash_the_loop(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Root incident this whole mechanism exists to prevent involved
        silent, hours-long gaps -- the prober itself must never go
        silent because one tick raised."""
        calls: list[int] = []

        def boom() -> dict[str, object]:
            calls.append(1)
            raise RuntimeError("boom")

        monkeypatch.setattr(budget_module, "read_windows", boom)
        monkeypatch.setattr(server, "USAGE_PROBE_TICK_S", 0.02)
        stop = threading.Event()
        t = threading.Thread(target=_usage_prober_loop, args=(_cfg(tmp_path), stop), daemon=True)
        t.start()
        for _ in range(200):
            if len(calls) >= 2:
                break
            time.sleep(0.01)
        stop.set()
        t.join(timeout=2)
        assert not t.is_alive()
        assert len(calls) >= 2  # survived at least one exception and ticked again

    def test_default_tick_is_within_the_1_to_5_minute_range(self) -> None:
        """The whole point of decoupling this from job-dispatch cadence:
        usage data should never be more than a few minutes stale."""
        assert 60.0 <= USAGE_PROBE_TICK_S <= 300.0
