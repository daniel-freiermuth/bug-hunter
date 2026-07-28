"""Tests for hunter.store.Store."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from hunter.store import Store
from hunter.types import Config, WindowState


@pytest.fixture
def store(tmp_path: Path) -> Store:
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    return Store(cfg)


def _make_finding(**overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "fingerprint": "repo:f.py:fn:logic",
        "file": "f.py",
        "symbol": "fn",
        "line": 10,
        "bug_class": "logic",
        "severity": "high",
        "confidence": 0.9,
        "summary": "Bug found",
        "detail": "Details here",
        "evidence_plan": "plan",
        "introduced_by": "abc123",
    }
    base.update(overrides)
    return base


# -- repos -----------------------------------------------------------------


class TestRepos:
    def test_add_and_get_by_id(self, store: Store) -> None:
        rid = store.add_repo("myrepo", "https://example.com/repo", "/tmp/repo")
        row = store.get_repo(rid)
        assert row is not None
        assert row["name"] == "myrepo"
        assert row["url"] == "https://example.com/repo"
        assert row["default_branch"] == "main"
        assert row["forge"] == "github"

    def test_get_by_name(self, store: Store) -> None:
        store.add_repo("myrepo", "https://example.com/repo", "/tmp/repo")
        row = store.get_repo("myrepo")
        assert row is not None
        assert row["name"] == "myrepo"

    def test_get_missing_returns_none(self, store: Store) -> None:
        assert store.get_repo(999) is None
        assert store.get_repo("nonexistent") is None

    def test_list_repos(self, store: Store) -> None:
        store.add_repo("alpha", "https://a", "/a")
        store.add_repo("beta", "https://b", "/b")
        repos = store.list_repos()
        assert len(repos) == 2
        assert repos[0]["name"] == "alpha"
        assert repos[1]["name"] == "beta"

    def test_add_repo_with_forge(self, store: Store) -> None:
        rid = store.add_repo("gl", "https://gl", "/gl", forge="gitlab")
        row = store.get_repo(rid)
        assert row is not None
        assert row["forge"] == "gitlab"

    def test_set_last_hunt(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.set_last_hunt(rid, "deadbeef")
        row = store.get_repo(rid)
        assert row is not None
        assert row["last_hunt_sha"] == "deadbeef"
        assert row["last_hunt_at"] is not None


# -- findings --------------------------------------------------------------


class TestFindings:
    def test_upsert_new(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, is_new = store.upsert_finding(rid, _make_finding())
        assert fid > 0
        assert is_new is True

    def test_upsert_duplicate(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, new1 = store.upsert_finding(rid, _make_finding())
        fid2, new2 = store.upsert_finding(rid, _make_finding())
        assert fid1 == fid2
        assert new1 is True
        assert new2 is False

    def test_get_finding(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        row = store.get_finding(fid)
        assert row is not None
        assert row["fingerprint"] == "repo:f.py:fn:logic"
        assert row["severity"] == "high"
        assert row["status"] == "new"

    def test_get_finding_missing(self, store: Store) -> None:
        assert store.get_finding(999) is None

    def test_list_findings_all(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp2"))
        assert len(store.list_findings()) == 2

    def test_list_findings_by_status(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp2"))
        store.set_status(fid, "queued")
        assert len(store.list_findings(status="queued")) == 1
        assert len(store.list_findings(status="new")) == 1

    def test_list_findings_by_repo_id(self, store: Store) -> None:
        r1 = store.add_repo("a", "https://a", "/a")
        r2 = store.add_repo("b", "https://b", "/b")
        store.upsert_finding(r1, _make_finding(fingerprint="fp1"))
        store.upsert_finding(r2, _make_finding(fingerprint="fp2"))
        assert len(store.list_findings(repo_id=r1)) == 1
        assert len(store.list_findings(repo_id=r2)) == 1


# -- set_status ------------------------------------------------------------


class TestSetStatus:
    def test_valid_status(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "queued")
        assert store.get_finding(fid)["status"] == "queued"

    def test_with_pr_url_and_rung(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "merged", pr_url="https://github.com/pr/1", rung=2)
        row = store.get_finding(fid)
        assert row["pr_url"] == "https://github.com/pr/1"
        assert row["rung_achieved"] == 2

    def test_with_verdict_reason(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "rejected", verdict_reason="not a real bug")
        row = store.get_finding(fid)
        assert row["verdict_reason"] == "not a real bug"

    def test_invalid_status_raises(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        with pytest.raises(ValueError, match="invalid status"):
            store.set_status(fid, "bogus")


# -- suppressions and known_active -----------------------------------------


class TestSuppressions:
    def test_suppressions(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))
        _fid3, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp3"))
        store.set_status(fid1, "rejected", verdict_reason="bad")
        store.set_status(fid2, "wontfix", verdict_reason="nope")
        # fid3 stays "new"
        supps = store.suppressions(rid)
        assert len(supps) == 2
        assert {s["id"] for s in supps} == {fid1, fid2}

    def test_known_active(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))
        store.set_status(fid2, "rejected", verdict_reason="bad")
        actives = store.known_active(rid)
        assert len(actives) == 1
        assert actives[0]["id"] == fid1

    def test_filters_by_repo(self, store: Store) -> None:
        r1 = store.add_repo("a", "https://a", "/a")
        r2 = store.add_repo("b", "https://b", "/b")
        store.upsert_finding(r1, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(r2, _make_finding(fingerprint="fp2"))
        store.set_status(fid2, "rejected", verdict_reason="no")
        assert len(store.suppressions(r1)) == 0
        assert len(store.suppressions(r2)) == 1
        assert len(store.known_active(r1)) == 1
        assert len(store.known_active(r2)) == 0


# -- jobs ------------------------------------------------------------------


class TestJobs:
    def test_create_and_list(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid, cap_tokens=100_000)
        assert jid > 0
        jobs = store.list_jobs()
        assert len(jobs) == 1
        assert jobs[0]["kind"] == "hunt"
        assert jobs[0]["state"] == "queued"
        assert jobs[0]["repo_name"] == "r"

    def test_update_job(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("fix", rid)
        store.update_job(jid, state="running", pid=12345)
        jobs = store.list_jobs()
        assert jobs[0]["state"] == "running"
        assert jobs[0]["pid"] == 12345

    def test_update_job_invalid_field(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("fix", rid)
        with pytest.raises(ValueError, match="invalid job fields"):
            store.update_job(jid, nonexistent="value")

    def test_create_job_with_finding(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        _jid = store.create_job("fix", rid, finding_id=fid)
        jobs = store.list_jobs()
        assert jobs[0]["finding_id"] == fid

    def test_list_jobs_limit(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        for _i in range(5):
            store.create_job("hunt", rid)
        assert len(store.list_jobs(limit=3)) == 3


# -- events ----------------------------------------------------------------


class TestEvents:
    def test_log_and_recent(self, store: Store) -> None:
        store.log_event("cycle", "started cycle 1")
        store.log_event("error", "something broke")
        events = store.recent_events()
        assert len(events) == 2
        # Most recent first
        assert events[0]["kind"] == "error"
        assert events[1]["kind"] == "cycle"

    def test_with_job_and_finding(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        jid = store.create_job("fix", rid, finding_id=fid)
        store.log_event("fix", "fixing", job_id=jid, finding_id=fid)
        events = store.recent_events()
        assert events[0]["job_id"] == jid
        assert events[0]["finding_id"] == fid

    def test_recent_events_limit(self, store: Store) -> None:
        for i in range(10):
            store.log_event("cycle", f"msg {i}")
        assert len(store.recent_events(limit=5)) == 5


# -- window log ------------------------------------------------------------


class TestWindowLog:
    def test_log_window(self, store: Store) -> None:
        states = [
            WindowState(
                limit_id="anthropic:5h",
                used_fraction=0.3,
                status="ok",
                resets_at=9999999,
                recorded_at=1000000,
                age_s=5.0,
            ),
            WindowState(
                limit_id="anthropic:7d",
                used_fraction=0.1,
                status="ok",
                resets_at=9999999,
                recorded_at=1000000,
                age_s=10.0,
            ),
        ]
        store.log_window(states)
        rows = store.db.execute("SELECT * FROM window_log ORDER BY id").fetchall()
        assert len(rows) == 2
        assert dict(rows[0])["limit_id"] == "anthropic:5h"
        assert dict(rows[1])["limit_id"] == "anthropic:7d"
        assert dict(rows[0])["source_age_s"] == 5


# -- update_finding_analysis -----------------------------------------------


class TestUpdateFindingAnalysis:
    def test_update_summary_only(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.update_finding_analysis(fid, summary="New summary")
        row = store.get_finding(fid)
        assert row["summary"] == "New summary"
        assert row["detail"] == "Details here"  # unchanged

    def test_update_multiple_fields(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.update_finding_analysis(fid, detail="New detail", confidence=0.5, severity="low")
        row = store.get_finding(fid)
        assert row["detail"] == "New detail"
        assert row["confidence"] == 0.5
        assert row["severity"] == "low"
        assert row["summary"] == "Bug found"  # unchanged

    def test_does_not_touch_status(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "queued")
        store.update_finding_analysis(fid, summary="Updated")
        row = store.get_finding(fid)
        assert row["status"] == "queued"


# -- pr_state --------------------------------------------------------------


class TestPrState:
    def test_upsert_and_get(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.upsert_pr_state(fid, pr_number=42, state="OPEN", head_ref="fix/bug")
        row = store.get_pr_state(fid)
        assert row is not None
        assert row["pr_number"] == 42
        assert row["state"] == "OPEN"
        assert row["head_ref"] == "fix/bug"

    def test_upsert_updates_existing(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.upsert_pr_state(fid, pr_number=42, state="OPEN")
        store.upsert_pr_state(fid, state="MERGED")
        row = store.get_pr_state(fid)
        assert row["state"] == "MERGED"
        assert row["pr_number"] == 42  # preserved from first insert

    def test_get_pr_state_missing(self, store: Store) -> None:
        assert store.get_pr_state(999) is None

    def test_upsert_pr_state_invalid_field(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        with pytest.raises(ValueError, match="invalid pr_state fields"):
            store.upsert_pr_state(fid, bad_field="x")

    def test_list_attention(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))
        fid3, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp3"))

        store.set_status(fid1, "pr_open")
        store.set_status(fid2, "pr_open")
        store.set_status(fid3, "new")  # not pr_open

        store.upsert_pr_state(fid1, pr_number=1, needs_attention="review_requested", synced_at=100)
        store.upsert_pr_state(fid2, pr_number=2, needs_attention=None, synced_at=200)
        store.upsert_pr_state(fid3, pr_number=3, needs_attention="stale", synced_at=50)

        attn = store.list_attention()
        # fid1: pr_open + needs_attention set → included
        # fid2: pr_open but needs_attention is NULL → excluded
        # fid3: needs_attention set but status isn't pr_open → excluded
        assert len(attn) == 1
        assert attn[0]["id"] == fid1

    def test_list_attention_sorted_by_synced_at(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))

        store.set_status(fid1, "pr_open")
        store.set_status(fid2, "pr_open")

        store.upsert_pr_state(fid1, pr_number=1, needs_attention="conflict", synced_at=200)
        store.upsert_pr_state(fid2, pr_number=2, needs_attention="review", synced_at=100)

        attn = store.list_attention()
        assert len(attn) == 2
        # Stalest sync first
        assert attn[0]["id"] == fid2
        assert attn[1]["id"] == fid1
