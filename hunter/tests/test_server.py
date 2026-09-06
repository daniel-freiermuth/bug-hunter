"""Tests for hunter.server._describe_cycle, _compute_sleep_s, and
_activity_status.

_describe_cycle classifies run_cycle's return shape into (state, detail)
for the Status page's "last log" line. _compute_sleep_s decides how long
the daemon loop sleeps after a cycle attempt. _activity_status is the
single canonical answer to "what is hunter doing right now" -- every
rendering surface (the activity panel, the manual-run button) must
derive its text from this and nothing else; see its docstring for the
four real incidents in one session that came from computing it more
than once, independently, in different places. All three are extracted
from the daemon loop / summary endpoint specifically so this logic is
provable by test rather than only observable by running the real
infinite loop or clicking around the UI.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter.server import PR_SYNC_INTERVAL_S, _activity_status, _compute_sleep_s, _describe_cycle
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
_ERROR_STATE = {"id": 1, "state": "error", "detail": "daemon loop crashed: boom", "next_wake_at": None, "updated_at": 0}
_IDLE_STATE = {"id": 1, "state": "idle", "detail": "last: nothing to do", "next_wake_at": None, "updated_at": 0}


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
