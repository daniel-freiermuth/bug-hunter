"""Regression: _record_job must save stdout_tail as notes for non-done jobs."""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter.scheduler import _record_job
from hunter.store import Store
from hunter.types import Config, RunResult


@pytest.fixture
def store(tmp_path: Path) -> Store:
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    return Store(cfg)


class TestRecordJobNotes:
    """stdout_tail must be captured in job notes for failed/killed runs."""

    def test_failed_job_gets_notes(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)
        rr = RunResult(
            exit_code=1,
            killed_reason=None,
            tokens_new=100,
            calls=2,
            session_file=None,
            duration_s=5.0,
            stdout_tail="some error context here",
        )
        state = _record_job(store, jid, rr)
        assert state == "failed"
        jobs = store.list_jobs()
        job = jobs[0]
        assert job["notes"] is not None, "notes must contain stdout_tail for failed jobs"
        assert "some error context" in job["notes"]

    def test_killed_job_gets_notes(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)
        rr = RunResult(
            exit_code=None,
            killed_reason="wallclock",
            tokens_new=50,
            calls=1,
            session_file=None,
            duration_s=1800.0,
            stdout_tail="timeout output",
        )
        state = _record_job(store, jid, rr)
        assert state == "killed"
        jobs = store.list_jobs()
        job = jobs[0]
        assert job["notes"] is not None, "notes must contain stdout_tail for killed jobs"
        assert "timeout output" in job["notes"]

    def test_done_job_no_notes(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)
        rr = RunResult(
            exit_code=0,
            killed_reason=None,
            tokens_new=100,
            calls=2,
            session_file=None,
            duration_s=5.0,
            stdout_tail="normal output",
        )
        state = _record_job(store, jid, rr)
        assert state == "done"
        jobs = store.list_jobs()
        job = jobs[0]
        assert job["notes"] is None, "done jobs should not get stdout_tail as notes"
