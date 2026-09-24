"""Tests for hunter.store.Store."""

from __future__ import annotations

import contextlib
import sqlite3
import threading
import time
from pathlib import Path
from typing import Any

import pytest

from hunter import server
from hunter.backends.omp_scavenge.capacity import WindowState
from hunter.store import Store, _require_keys
from hunter.types import Config, SchedulerStateDict, now_ms


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

    def test_delete_repo_with_no_history(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.soft_delete_repo(rid)
        assert store.get_repo(rid) is None
        # Invisible, but still holding its id until its files are gone.
        assert store.deleted_repo_ids() == [rid]
        store.forget_deleted_repo(rid)
        assert store.deleted_repo_ids() == []

    def test_delete_repo_refuses_with_findings(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding())
        with pytest.raises(ValueError, match="finding"):
            store.soft_delete_repo(rid)
        assert store.get_repo(rid) is not None

    def test_delete_repo_refuses_with_jobs(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.create_job("hunt", rid)
        with pytest.raises(ValueError, match="job"):
            store.soft_delete_repo(rid)
        assert store.get_repo(rid) is not None

    def test_delete_repo_missing_is_noop(self, store: Store) -> None:
        store.soft_delete_repo(999)  # no rows affected, does not raise
        assert store.deleted_repo_ids() == []

    def test_create_job_refuses_a_soft_deleted_repo(self, store: Store) -> None:
        """The scheduler picks a live repo, the operator deletes it
        mid-cycle, and the runner reaches the job insert afterwards. The
        foreign key is no defence -- the flagged row is still physically
        there -- so a job written here would pin the repo half-deleted
        forever: forget_deleted_repo is a plain DELETE and the FK refuses
        it while any job references the row.
        """
        live = store.add_repo("live", "https://l", "/l")
        doomed = store.add_repo("doomed", "https://d", "/d")
        picked = store.get_repo(doomed)  # the scheduler's view, taken while live
        assert picked is not None
        store.soft_delete_repo(doomed)

        with pytest.raises(ValueError, match=f"repo {doomed} is deleted"):
            store.create_job("hunt", picked["id"], state="running")
        assert store.list_jobs() == []
        # Keyed on deleted_at, not on absence: a live repo still gets its job.
        assert store.create_job("hunt", live, state="running") > 0

        # And the deletion can still finish, because no job row is holding
        # the id: the reaper's DELETE goes through instead of hitting the FK.
        assert store.deleted_repo_ids() == [doomed]
        store.forget_deleted_repo(doomed)
        assert store.deleted_repo_ids() == []
        assert store.get_repo(doomed) is None

    def test_legacy_name_based_paths_are_rewritten_to_ids(self, tmp_path: Path) -> None:
        """Rows written before clone dirs were keyed by id must converge.

        Both daemons share one work_root, so if Python kept pointing at
        repos/<name> while hunter-rs points at repos/repo-<id>, a
        rollback would re-clone every repo. Mirrors migration 007.
        """
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "t.db")
        store = Store(cfg)
        rid = store.add_repo("widget", "https://w", tmp_path / "repos")
        # Put the row back the way the pre-migration code wrote it.
        store.db.execute(
            "UPDATE repos SET path = ? WHERE id = ?",
            (str(tmp_path / "repos" / "widget"), rid),
        )
        store.db.commit()

        reopened = Store(cfg)  # runs the migrations again
        assert reopened.get_repo(rid)["path"] == str(tmp_path / "repos" / f"repo-{rid}")

        # Idempotent: a second open must not mangle the already-migrated path.
        assert Store(cfg).get_repo(rid)["path"] == str(tmp_path / "repos" / f"repo-{rid}")

    def test_repo_is_deleted_only_while_reclamation_is_pending(self, tmp_path: Path) -> None:
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "t.db")
        store = Store(cfg)
        rid = store.add_repo("gone", "https://g", tmp_path / "repos")
        assert store.repo_is_deleted(rid) is False
        store.soft_delete_repo(rid)
        assert store.repo_is_deleted(rid) is True
        store.forget_deleted_repo(rid)
        assert store.repo_is_deleted(rid) is False

    def test_soft_delete_holds_the_write_lock_across_its_checks(self, tmp_path: Path) -> None:
        """The history check and the flag must see one state.

        `with self.db` only commits -- sqlite3 opens a transaction lazily
        on the first write -- so the COUNT queries used to run outside any
        transaction. A job inserted by another connection in that gap was
        invisible to the check, and the repo got flagged anyway; the
        deletion then failed much later, inside the reaper, as an
        IntegrityError on a thread with no handler for it.
        """
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "t.db")
        store = Store(cfg)
        rid = store.add_repo("r", "https://r", tmp_path / "repos")

        other = Store(cfg)  # separate connection, as a handler thread has
        started = threading.Event()
        blocked: list[str] = []

        class _Probe:
            """Delegates to the real connection; probes for the write lock
            from another connection while the history check is running."""

            def __init__(self, real: object) -> None:
                self._real = real

            def __getattr__(self, name: str) -> object:
                return getattr(self._real, name)

            def __enter__(self) -> object:
                return self._real.__enter__()  # type: ignore[attr-defined]

            def __exit__(self, *exc: object) -> object:
                return self._real.__exit__(*exc)  # type: ignore[attr-defined]

            def execute(self, sql: str, *args: object) -> object:
                result = self._real.execute(sql, *args)  # type: ignore[attr-defined]
                if sql.startswith("SELECT COUNT(*) AS n FROM findings"):
                    started.set()
                    try:
                        other.db.execute("BEGIN IMMEDIATE")
                        blocked.append("acquired")
                        other.db.rollback()
                    except sqlite3.OperationalError as exc:
                        blocked.append(f"blocked: {exc}")
                return result

        store.db = _Probe(store.db)  # type: ignore[assignment]
        store.soft_delete_repo(rid)

        assert started.is_set(), "the check never ran"
        assert blocked, "the probe never ran"
        assert blocked[0].startswith("blocked"), (
            "another connection acquired the write lock during the history "
            f"check, so a job could have been inserted into the gap: {blocked}"
        )

    def test_notes_lock_is_shared_across_store_instances(self, tmp_path: Path) -> None:
        """ThreadingHTTPServer creates one Store (and one SQLite connection)
        per handler thread. The notes-file lock must be class-level so it
        actually serializes across those independent instances, not just
        within a single instance/connection."""
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
        store1 = Store(cfg)
        store2 = Store(cfg)
        assert store1._NOTES_LOCK is store2._NOTES_LOCK

    def test_reclamation_blocks_concurrent_append_repo_note(self, tmp_path: Path) -> None:
        """Reclaiming a deleted repo must block append_repo_note() on a
        different Store/connection until it finishes.

        Deletion is two-phase now, so the flag itself is atomic and needs
        no protection. The dangerous window moved to reclamation: a note
        written between the rmtree and the row deletion recreates the
        directory after the reaper has decided it is gone, and the row is
        then dropped with files still on disk -- exactly the state that
        lets a later repo inherit them.

        Mirrors ThreadingHTTPServer: each handler thread gets its own
        Store (own SQLite connection), created in that thread.
        """
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
        setup_store = Store(cfg)
        rid = setup_store.add_repo("r", "https://r", tmp_path / "repos")
        setup_store.append_repo_note(rid, "original note")
        setup_store.soft_delete_repo(rid)

        order: list[str] = []
        release = threading.Event()

        class _SlowDb:
            """Delegates to a real connection; blocks after the row is
            dropped so the test can observe reclamation mid-flight."""

            def __init__(self, real: object) -> None:
                self._real = real

            def __getattr__(self, name: str) -> object:
                return getattr(self._real, name)

            # Dunders are looked up on the type, so __getattr__ never sees
            # them; forget_deleted_repo uses `with self.db:`.
            def __enter__(self) -> object:
                return self._real.__enter__()  # type: ignore[attr-defined]

            def __exit__(self, *exc: object) -> object:
                return self._real.__exit__(*exc)  # type: ignore[attr-defined]

            def execute(self, query: str, *args: object) -> object:
                result = self._real.execute(query, *args)  # type: ignore[attr-defined]
                if query.startswith("DELETE FROM repos"):
                    order.append("reap:entered")
                    release.wait(timeout=2)
                    order.append("reap:resumed")
                return result

        def do_reap() -> None:
            store_a = Store(cfg)
            store_a.db = _SlowDb(store_a.db)  # type: ignore[assignment]
            server.reap_repo(store_a, tmp_path / "repos", rid)

        def do_append() -> None:
            store_b = Store(cfg)
            with contextlib.suppress(ValueError):
                store_b.append_repo_note(rid, "racy note")
            order.append("append:done")

        t1 = threading.Thread(target=do_reap)
        t1.start()
        for _ in range(200):
            if "reap:entered" in order:
                break
            time.sleep(0.01)
        assert "reap:entered" in order, "reap_repo never reached its DELETE"

        t2 = threading.Thread(target=do_append)
        t2.start()
        for _ in range(20):
            assert "append:done" not in order
            time.sleep(0.01)

        release.set()
        t1.join(timeout=2)
        t2.join(timeout=2)
        assert order.index("reap:resumed") < order.index("append:done")


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

    def test_list_findings_min_severity_high(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding(fingerprint="fp1", severity="low"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp2", severity="medium"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp3", severity="high"))
        assert len(store.list_findings(min_severity="high")) == 1

    def test_list_findings_min_severity_medium(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding(fingerprint="fp1", severity="low"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp2", severity="medium"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp3", severity="high"))
        assert len(store.list_findings(min_severity="medium")) == 2

    def test_list_findings_min_severity_low(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding(fingerprint="fp1", severity="low"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp2", severity="medium"))
        store.upsert_finding(rid, _make_finding(fingerprint="fp3", severity="high"))
        assert len(store.list_findings(min_severity="low")) == 3

    def test_list_findings_min_severity_combined_with_status(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1", severity="high"))
        _fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2", severity="high"))
        store.set_status(fid1, "queued")
        # fid2 stays "new"
        assert len(store.list_findings(status="new", min_severity="high")) == 1

    def test_list_findings_by_type(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding(fingerprint="fp1"), finding_type="bug")
        store.upsert_finding(
            rid,
            {
                "fingerprint": "fp2",
                "ecosystem": "npm",
                "package": "foo",
                "current_version": "1.0.0",
                "latest_version": "2.0.0",
                "update_type": "major",
                "severity": "medium",
                "confidence": 1.0,
                "summary": "foo is outdated",
            },
            finding_type="dep_update",
        )
        assert len(store.list_findings()) == 2
        assert len(store.list_findings(finding_type="bug")) == 1
        assert len(store.list_findings(finding_type="dep_update")) == 1
        assert store.list_findings(finding_type="bug")[0]["type"] == "bug"

    def test_list_findings_computes_category_per_type(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.upsert_finding(rid, _make_finding(fingerprint="fp1"), finding_type="bug")
        store.upsert_finding(
            rid,
            {
                "fingerprint": "fp2",
                "ecosystem": "npm",
                "package": "foo",
                "current_version": "1.0.0",
                "latest_version": "2.0.0",
                "update_type": "major",
                "severity": "medium",
                "confidence": 1.0,
                "summary": "foo is outdated",
            },
            finding_type="dep_update",
        )
        rows = {r["type"]: r for r in store.list_findings()}
        assert rows["bug"]["category"] == "logic"  # from bug_class
        assert rows["dep_update"]["category"] == "major"  # from update_type


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

    def test_filters_by_finding_type(self, store: Store) -> None:
        """Bug-hunt suppression/known corpora must not leak dep_update/test_gap/
        refactor findings sharing the same repo -- they use separate playbooks
        and a mixed-type corpus would confuse the bug hunter."""
        rid = store.add_repo("r", "https://r", "/r")
        bug_fid, _ = store.upsert_finding(
            rid, _make_finding(fingerprint="fp-bug"), finding_type="bug"
        )
        dep_fid, _ = store.upsert_finding(
            rid, {"fingerprint": "fp-dep", "package": "foo"}, finding_type="dep_update"
        )
        store.set_status(bug_fid, "rejected", verdict_reason="bad")
        store.set_status(dep_fid, "rejected", verdict_reason="also bad")
        assert [s["id"] for s in store.suppressions(rid)] == [bug_fid]
        assert [s["id"] for s in store.suppressions(rid, finding_type="dep_update")] == [dep_fid]

        other_fid, _ = store.upsert_finding(
            rid, _make_finding(fingerprint="fp-bug-2"), finding_type="bug"
        )
        assert [a["id"] for a in store.known_active(rid)] == [other_fid]


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
    def test_log_window_observation(self, store: Store) -> None:
        store.log_window_observation(
            "anthropic:5h",
            0.3,
            "ok",
            9999999,
            5.0,
        )
        store.log_window_observation(
            "anthropic:7d",
            0.1,
            "ok",
            9999999,
            10.0,
        )
        rows = store.db.execute("SELECT * FROM window_log ORDER BY id").fetchall()
        assert len(rows) == 2
        assert dict(rows[0])["limit_id"] == "anthropic:5h"
        assert dict(rows[1])["limit_id"] == "anthropic:7d"
        assert dict(rows[0])["source_age_s"] == 5


# -- calibration -------------------------------------------------------


class TestCalibration:
    """Backend._observe calibration side effect and estimate_capacity."""

    _RESETS_AT = 99_999_999_999

    def _probe(self, used_fraction: float) -> WindowState:
        return WindowState(
            limit_id="anthropic:5h",
            used_fraction=used_fraction,
            status="ok",
            resets_at=self._RESETS_AT,
            recorded_at=1,
            age_s=1.0,
        )

    def _observe(self, store: Store, probe: WindowState) -> None:
        """Simulate backend._observe for a single probe."""
        from hunter.backends.omp_scavenge.facade import OmpScavengeBackend  # noqa: PLC0415

        cfg = Config(work_root=Path("/tmp"), db_path=Path("/tmp/test.db"))
        backend = OmpScavengeBackend(cfg=cfg, ledger=store)
        backend._observe({"anthropic:5h": probe})

    def test_first_probe_records_no_sample(self, store: Store) -> None:
        """Nothing to compare against yet -- no prior row for this window."""
        self._observe(store, self._probe(0.10))
        assert store.db.execute("SELECT COUNT(*) c FROM calibration_samples").fetchone()["c"] == 0

    def test_fresh_probe_with_hunter_spend_records_a_sample(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        self._observe(store, self._probe(0.10))
        time.sleep(0.02)
        jid = store.create_job("hunt", rid)
        store.update_job(jid, state="done", tokens_new=500_000, finished_at=now_ms())
        time.sleep(0.02)
        self._observe(store, self._probe(0.20))

        rows = store.db.execute("SELECT * FROM calibration_samples").fetchall()
        assert len(rows) == 1
        row = dict(rows[0])
        assert row["limit_id"] == "anthropic:5h"
        assert row["hunter_tokens"] == 500_000
        assert row["used_fraction_delta"] == pytest.approx(0.10)
        assert row["window_resets_at"] == self._RESETS_AT

    def test_unchanged_used_fraction_records_no_sample(self, store: Store) -> None:
        """A re-read of the same stale probe (used_fraction didn't move)
        must not fabricate a sample out of noise."""
        rid = store.add_repo("r", "https://r", "/r")
        self._observe(store, self._probe(0.10))
        time.sleep(0.02)
        jid = store.create_job("hunt", rid)
        store.update_job(jid, state="done", tokens_new=500_000, finished_at=now_ms())
        time.sleep(0.02)
        self._observe(store, self._probe(0.10))  # same fraction -- no fresh probe
        assert store.db.execute("SELECT COUNT(*) c FROM calibration_samples").fetchone()["c"] == 0

    def test_no_hunter_spend_records_no_sample(self, store: Store) -> None:
        """used_fraction moved but hunter didn't run anything in the gap
        (e.g. a human's own interactive usage) -- nothing attributable."""
        self._observe(store, self._probe(0.10))
        time.sleep(0.02)
        self._observe(store, self._probe(0.20))
        assert store.db.execute("SELECT COUNT(*) c FROM calibration_samples").fetchone()["c"] == 0

    def test_different_window_instance_not_compared(self, store: Store) -> None:
        """A genuinely new window (different resets_at) must not be
        diffed against the previous instance's used_fraction."""
        rid = store.add_repo("r", "https://r", "/r")
        self._observe(store, self._probe(0.90))  # old window, nearly full
        time.sleep(0.02)
        jid = store.create_job("hunt", rid)
        store.update_job(jid, state="done", tokens_new=500_000, finished_at=now_ms())
        time.sleep(0.02)
        fresh = WindowState(
            limit_id="anthropic:5h",
            used_fraction=0.05,
            status="ok",
            resets_at=self._RESETS_AT + 6 * 3600 * 1000,
            recorded_at=1,
            age_s=1.0,
        )
        self._observe(store, fresh)
        assert store.db.execute("SELECT COUNT(*) c FROM calibration_samples").fetchone()["c"] == 0

    def test_estimate_capacity_no_data_returns_none(self, store: Store) -> None:
        assert store.estimate_capacity("anthropic:5h") is None

    def test_estimate_capacity_returns_max_spend_per_cycle(self, store: Store) -> None:
        """Capacity = max tokens hunter spent in any single window cycle."""
        now = now_ms()
        rid = store.add_repo("r", "https://r", "/r")
        five_h = 5 * 3600 * 1000
        # Two completed 5h cycles with different spend levels
        resets1 = now - five_h  # one cycle ago
        resets2 = now - 2 * five_h  # two cycles ago
        for ra in (resets1, resets2):
            store.db.execute(
                "INSERT INTO window_log (observed_at, limit_id,"
                " used_fraction, status, resets_at, source_age_s)"
                " VALUES (?, 'anthropic:5h', 0.5, 'ok', ?, 60)",
                (ra - 1000, ra),
            )
        # Cycle 1: 500k tokens
        jid1 = store.create_job("hunt", rid)
        store.update_job(jid1, state="done", tokens_new=500_000, finished_at=resets1 - 1000)
        # Cycle 2: 2M tokens (the max)
        jid2 = store.create_job("hunt", rid)
        store.update_job(jid2, state="done", tokens_new=2_000_000, finished_at=resets2 - 1000)
        store.db.commit()

        cap = store.estimate_capacity("anthropic:5h")
        assert cap == 2_000_000  # max of the two cycles

    def test_estimate_capacity_scoped_by_limit_id(self, store: Store) -> None:
        """5h and 7d use separate window_log entries."""
        now = now_ms()
        rid = store.add_repo("r", "https://r", "/r")
        resets = now - 7 * 24 * 3600 * 1000
        store.db.execute(
            "INSERT INTO window_log (observed_at, limit_id,"
            " used_fraction, status, resets_at, source_age_s)"
            " VALUES (?, 'anthropic:7d', 0.5, 'ok', ?, 60)",
            (resets - 1000, resets),
        )
        jid = store.create_job("hunt", rid)
        store.update_job(jid, state="done", tokens_new=1_000_000, finished_at=resets - 500)
        store.db.commit()

        assert store.estimate_capacity("anthropic:5h") is None  # no 5h cycles
        assert store.estimate_capacity("anthropic:7d") == 1_000_000


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

    def test_list_attention_prefers_attention_since_over_synced_at(self, store: Store) -> None:
        """Regression: sync_prs bulk-refreshes synced_at for EVERY pr_open
        finding every cycle in a fixed order, so it reflects loop
        iteration order, not genuine wait time (observed live: a
        higher-id finding won 5 straight tie-breaks over one that had
        actually been waiting far longer). attention_since is the real
        fairness key and must win whenever both are present."""
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))

        store.set_status(fid1, "pr_open")
        store.set_status(fid2, "pr_open")

        # fid1 has been flagged since T=1 (long-waiting) but was JUST
        # resynced (synced_at=999, looks fresh by the old key).
        store.upsert_pr_state(
            fid1, pr_number=1, needs_attention="checks_failing", attention_since=1, synced_at=999
        )
        # fid2 was flagged much more recently (T=500) but happened to
        # sync a moment earlier in this cycle's pass (synced_at=100).
        store.upsert_pr_state(
            fid2, pr_number=2, needs_attention="checks_failing", attention_since=500, synced_at=100
        )

        attn = store.list_attention()
        assert len(attn) == 2
        assert attn[0]["id"] == fid1  # genuinely longest-waiting, despite the fresher synced_at
        assert attn[1]["id"] == fid2

    def test_list_pending_harvest(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))
        fid3, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp3"))

        store.set_status(fid1, "merged")
        store.set_status(fid2, "merged")
        store.set_status(fid3, "pr_open")  # not merged yet

        store.upsert_pr_state(fid1, pr_number=1, state="MERGED", synced_at=100)
        store.upsert_pr_state(fid2, pr_number=2, state="MERGED", synced_at=200, harvested_at=999)
        store.upsert_pr_state(fid3, pr_number=3, state="OPEN", synced_at=50)

        pending = store.list_pending_harvest()
        # fid1: merged + harvested_at NULL → included
        # fid2: merged but already harvested → excluded
        # fid3: not merged → excluded
        assert len(pending) == 1
        assert pending[0]["id"] == fid1

    def test_list_pending_harvest_sorted_by_synced_at(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid1, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp1"))
        fid2, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp2"))

        store.set_status(fid1, "merged")
        store.set_status(fid2, "merged")

        store.upsert_pr_state(fid1, pr_number=1, state="MERGED", synced_at=200)
        store.upsert_pr_state(fid2, pr_number=2, state="MERGED", synced_at=100)

        pending = store.list_pending_harvest()
        assert len(pending) == 2
        # Oldest merge first
        assert pending[0]["id"] == fid2
        assert pending[1]["id"] == fid1


# -- reconcile_orphaned_jobs -------------------------------------------------


class TestReconcileOrphanedJobs:
    """Regression: a daemon that dies mid-run_fix (crash, systemctl
    restart, or an in-process exception run_cycle's catch-all swallowed)
    must not leave a job stuck 'running' forever and its finding stuck
    'fixing' forever -- 'fixing' is never scanned by the normal work
    queue, so without reconciliation it vanishes indefinitely (observed
    in production: a job sat 'running' for a month after an old crash)."""

    def test_orphaned_fix_job_resets_finding_to_queued(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "fixing")
        jid = store.create_job("fix", rid, finding_id=fid, cap_tokens=150_000)
        store.update_job(jid, state="running")

        result = store.reconcile_orphaned_jobs()

        assert [f["id"] for f in result["findings"]] == [fid]
        assert [j["id"] for j in result["jobs"]] == [jid]
        job = store.list_jobs()[0]
        assert job["state"] == "killed"
        assert job["killed_reason"] == "orphaned"
        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["status"] == "queued"

    def test_finding_stuck_fixing_with_already_terminal_job_is_still_recovered(
        self, store: Store
    ) -> None:
        """run_fix records the job's terminal state (_record_job) BEFORE
        the git-push/PR-create/salvage code that follows it -- an
        exception anywhere in that later stretch leaves the job row
        already terminal while the finding is still stuck 'fixing'. A
        job-state-based check alone would miss this; reconciliation must
        key off findings.status directly."""
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "fixing")
        jid = store.create_job("fix", rid, finding_id=fid, cap_tokens=150_000)
        store.update_job(jid, state="done")  # _record_job already ran fine

        result = store.reconcile_orphaned_jobs()

        assert [f["id"] for f in result["findings"]] == [fid]
        assert result["jobs"] == []  # nothing 'running' -- job row untouched
        assert store.list_jobs()[0]["state"] == "done"
        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["status"] == "queued"

    def test_fixing_finding_recovered_regardless_of_orphaned_jobs_kind(self, store: Store) -> None:
        """reconcile_orphaned_jobs recovers findings.status == 'fixing' by
        querying findings directly -- it never joins through jobs.kind or
        jobs.finding_id. A concurrently orphaned 'hunt' job (which never
        carries a finding_id) must not suppress recovery of an unrelated
        finding stuck at 'fixing' in the same pass."""
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "fixing")
        jid = store.create_job("hunt", rid)
        store.update_job(jid, state="running")

        result = store.reconcile_orphaned_jobs()

        assert [f["id"] for f in result["findings"]] == [fid]
        assert [j["id"] for j in result["jobs"]] == [jid]
        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["status"] == "queued"
        assert store.list_jobs()[0]["state"] == "killed"

    def test_finding_already_resolved_is_left_alone(self, store: Store) -> None:
        """A finding no longer at 'fixing' (resolved via a later, separate
        attempt) must never be touched, even if a stale job row for it is
        still 'running'."""
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        jid = store.create_job("fix", rid, finding_id=fid, cap_tokens=150_000)
        store.update_job(jid, state="running")
        store.set_status(fid, "merged")  # resolved independently in the meantime

        result = store.reconcile_orphaned_jobs()

        assert result["findings"] == []
        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["status"] == "merged"

    def test_nothing_stuck_is_a_noop(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)
        store.update_job(jid, state="done")

        result = store.reconcile_orphaned_jobs()

        assert result == {"findings": [], "jobs": []}
        assert store.list_jobs()[0]["state"] == "done"


# -- current_job / scheduler_state -------------------------------------------


class TestCurrentJob:
    def test_none_when_nothing_running(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        store.create_job("hunt", rid)  # queued, not running
        assert store.current_job() is None

    def test_create_job_with_state_running_is_visible_immediately(self, store: Store) -> None:
        """Regression: run_* functions used to create a job at the
        'queued' default, do real prep work (prompt building, git
        operations), and only THEN call update_job(state="running").
        During that gap, _cycle_lock was already held (the UI's "cycle
        running" indicator) but current_job() found nothing, showing
        stale last-cycle text at the same time as a "running" badge.
        Passing state="running" to create_job closes the gap entirely --
        no separate update_job call needed, and never a window."""
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid, cap_tokens=200_000, state="running")
        job = store.current_job()
        assert job is not None
        assert job["id"] == jid
        assert job["state"] == "running"

    def test_returns_the_running_job_with_repo_and_finding_context(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-1"))
        jid = store.create_job("fix", rid, finding_id=fid, cap_tokens=150_000)
        store.update_job(jid, state="running")
        job = store.current_job()
        assert job is not None
        assert job["id"] == jid
        assert job["repo_name"] == "r"
        assert job["finding_fingerprint"] == "fp-1"
        assert job["finding_summary"] == "Bug found"

    def test_only_the_most_recent_running_job_is_returned(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        j1 = store.create_job("hunt", rid)
        store.update_job(j1, state="done")
        j2 = store.create_job("hunt", rid)
        store.update_job(j2, state="running")
        job = store.current_job()
        assert job is not None
        assert job["id"] == j2


class TestSchedulerState:
    def test_none_before_ever_set(self, store: Store) -> None:
        assert store.get_scheduler_state() is None

    def test_set_and_get_roundtrip(self, store: Store) -> None:
        store.set_scheduler_state("idle", "no work", 12345)
        state = store.get_scheduler_state()
        assert state is not None
        assert state["state"] == "idle"
        assert state["detail"] == "no work"
        assert state["next_wake_at"] == 12345

    def test_set_overwrites_previous_value(self, store: Store) -> None:
        store.set_scheduler_state("idle", "no work", 100)
        store.set_scheduler_state("denied", "5h ramp", 200)
        state = store.get_scheduler_state()
        assert state is not None
        assert state["state"] == "denied"
        assert state["detail"] == "5h ramp"
        assert state["next_wake_at"] == 200


# ---------------------------------------------------------------------------
# _require_keys: the one runtime check at the SQL-row-to-TypedDict seam
# ---------------------------------------------------------------------------


class TestRequireKeys:
    """No static type checker can verify a SQL query's actual result
    columns match a declared TypedDict -- that's a runtime fact about
    the database, not something mypy parses. _require_keys is the one
    deliberate runtime check standing at that exact seam, so a
    schema/query drift fails immediately and clearly, in the store
    layer where the row was actually built, rather than as a confusing
    KeyError far downstream in unrelated consuming code."""

    def test_passes_through_when_all_keys_present(self) -> None:
        row = {"id": 1, "state": "idle", "detail": "d", "next_wake_at": None, "updated_at": 0}
        result = _require_keys(
            row, "id", "state", "detail", "next_wake_at", "updated_at", shape=SchedulerStateDict
        )
        assert result == row

    def test_raises_with_exact_missing_keys_and_actual_shape(self) -> None:
        row = {"id": 1, "state": "error"}  # simulates a schema/query drift
        with pytest.raises(ValueError, match=r"detail.*next_wake_at.*updated_at") as exc_info:
            _require_keys(
                row, "id", "state", "detail", "next_wake_at", "updated_at", shape=SchedulerStateDict
            )
        assert "SchedulerStateDict" in str(exc_info.value)
        assert "['id', 'state']" in str(exc_info.value), (
            "must name what WAS present, not just what's missing"
        )

    def test_extra_unexpected_keys_do_not_fail(self) -> None:
        """Not every column needs a home in the TypedDict -- current_job()
        deliberately carries DB columns the frontend never reads. Only
        missing REQUIRED keys are an error."""
        row = {
            "id": 1,
            "state": "idle",
            "detail": "d",
            "next_wake_at": None,
            "updated_at": 0,
            "some_future_column": "unexpected but harmless",
        }
        result = _require_keys(
            row, "id", "state", "detail", "next_wake_at", "updated_at", shape=SchedulerStateDict
        )
        assert result["some_future_column"] == "unexpected but harmless"  # type: ignore[typeddict-item]


def test_standards_finding_round_trips_through_the_fallback(store: Store) -> None:
    """The rollback must not lose a field the primary path keeps.

    `standard_section` is what a `standards` finding cites, and the Rust
    daemon persists it and exposes it as the computed `category`. The
    Python store gained the column via migration but neither wrote it on
    insert nor mapped it on read, so a fallback ingest would have stored
    NULL and served a standards finding with no category — silently
    dropping the one field that type exists to record.
    """
    repo_id = store.add_repo(
        "widget", "https://example.com/widget.git", store.cfg.work_root / "repos"
    )
    section = "Type safety / Domain types over primitives"
    fid, created = store.upsert_finding(
        repo_id,
        {
            "fingerprint": "widget:src/lib.rs:parse:type-safety",
            "file": "src/lib.rs",
            "severity": "medium",
            "confidence": 0.9,
            "summary": "raw String where a domain type is specified",
            "standard_section": section,
            "current_approach": "takes a String",
            "proposed_approach": "takes a RepoName",
        },
        finding_type="standards",
    )
    assert created

    stored = store.get_finding(fid)
    assert stored is not None
    assert stored["standard_section"] == section, "the cited standard must be persisted"

    listed = [r for r in store.list_findings() if r["id"] == fid]
    assert listed, "the finding must be listed"
    assert listed[0]["category"] == section, (
        "standards findings must expose their section as category, like every other type"
    )


class TestRunningEstimate:
    """The inflight reservation -- what a running job has been promised
    but not yet spent."""

    def test_a_running_job_is_counted_by_its_estimate_not_its_cap(self, store: Store) -> None:
        """The reservation is what the ramp set aside for the job, not
        the kill threshold it was granted. The two are independent
        numbers -- the cap stops bounding spend at all once headroom is
        the only bound -- so summing caps under-reserves for a job that
        is very much running.
        """
        rid = store.add_repo("r", "https://r", "/r")
        store.create_job("hunt", rid, cap_tokens=20_000, state="running", estimated_tokens=204_000)

        assert store.running_estimate() == 204_000

    def test_a_row_without_an_estimate_still_contributes_its_cap(self, store: Store) -> None:
        """Jobs written before estimated_tokens existed have no estimate
        to recover, so they keep contributing their cap -- what the
        reservation meant for them at the time. A daemon restarted
        mid-job across the migration would otherwise reserve nothing for
        the job it left running.
        """
        rid = store.add_repo("r", "https://r", "/r")
        # No estimate recorded: exactly the shape of a pre-migration row.
        store.create_job("hunt", rid, cap_tokens=50_000, state="running")
        store.create_job("fix", rid, cap_tokens=20_000, state="running", estimated_tokens=204_000)

        assert store.running_estimate() == 254_000


class TestLedgerQueriesUseThePartialIndex:
    """The budget sums must not scan the jobs table.

    jobs_finished_at is declared WHERE finished_at IS NOT NULL AND
    tokens_new IS NOT NULL, so a query that does not carry the second
    predicate cannot use it -- SQLite has no way to prove the query only
    touches indexed rows, and falls back to scanning every job. The
    predicate does not change the sums, since SUM skips NULLs.

    The plan is taken from the SQL the methods actually execute, not
    from a copy pasted into the test: a copy stays green when the real
    query loses the predicate, which is the only regression this is
    here to catch.
    """

    @staticmethod
    def _captured_plan(store: Store, call: str) -> list[str]:
        # The trace callback reports each statement with its parameters
        # already bound, so the plan below is of the statement the method
        # really ran -- there is no copy of the SQL here to drift.
        seen: list[str] = []
        store.db.set_trace_callback(seen.append)
        try:
            if call == "since":
                store.finished_since(0)
            else:
                store.finished_between(0, now_ms())
        finally:
            store.db.set_trace_callback(None)

        sql = next(s for s in seen if "SUM(tokens_new)" in s)
        rows = store.db.execute("EXPLAIN QUERY PLAN " + sql).fetchall()
        return [r["detail"] for r in rows]

    def test_finished_since_searches_the_index(self, store: Store) -> None:
        plan = self._captured_plan(store, "since")
        assert any("jobs_finished_at" in d for d in plan), plan
        assert not any(d.startswith("SCAN jobs") for d in plan), plan

    def test_finished_between_searches_the_index(self, store: Store) -> None:
        plan = self._captured_plan(store, "between")
        assert any("jobs_finished_at" in d for d in plan), plan
        assert not any(d.startswith("SCAN jobs") for d in plan), plan


class TestResponseShapesMatchRust:
    """Internal columns must not leak into API responses.

    The Python reads are `SELECT *` while the Rust port names its columns
    one by one, so any column added for the daemon's own use appears on
    one daemon's API responses and not the other's. The frontend is
    served by whichever daemon is running, so the two shapes have to
    agree.
    """

    def test_found_by_job_is_not_in_the_findings_shape(self, store: Store) -> None:
        rid = store.add_repo(
            "w", "https://github.com/a/w.git", store.cfg.work_root / "repos", "main", "github"
        )
        jid = store.create_job("hunt", rid, None, 1000, "running")
        fid, inserted = store.upsert_finding(
            rid, _make_finding(), finding_type="bug", found_by_job=jid
        )
        assert inserted

        # The column is written -- this is not passing because nothing set it.
        row = store.db.execute("SELECT found_by_job FROM findings WHERE id = ?", (fid,)).fetchone()
        assert row["found_by_job"] == jid

        assert "found_by_job" not in dict(store.get_finding(fid) or {})
        assert all("found_by_job" not in f for f in store.list_findings())

        # And the provenance is still reachable, the other way round.
        entry = next(j for j in store.list_jobs(limit=10) if j["id"] == jid)
        assert entry["produced_finding_ids"] == [fid]

    def test_estimated_tokens_is_not_in_the_jobs_shape(self, store: Store) -> None:
        rid = store.add_repo(
            "w", "https://github.com/a/w.git", store.cfg.work_root / "repos", "main", "github"
        )
        fid, _ = store.upsert_finding(rid, _make_finding())
        jid = store.create_job(
            "fix", rid, finding_id=fid, cap_tokens=1000, state="running", estimated_tokens=204_000
        )

        # The column is written -- this is not passing because nothing set it.
        row = store.db.execute("SELECT estimated_tokens FROM jobs WHERE id = ?", (jid,)).fetchone()
        assert row["estimated_tokens"] == 204_000

        assert all("estimated_tokens" not in j for j in store.list_jobs())
        assert all("estimated_tokens" not in j for j in store.jobs_by_finding(fid))
        assert "estimated_tokens" not in (store.current_job() or {})
