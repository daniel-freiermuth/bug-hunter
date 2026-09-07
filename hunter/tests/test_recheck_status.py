"""Regression: run_recheck must leave status at 'rechecking' on infra/execution
failures (repo missing, git sync failure, inconclusive verdict) so the next
run_cycle's priority scan retries it automatically -- exactly like a killed
run_fix leaves a finding at 'queued'. Resetting to 'new' on these paths was
itself a bug: a finding that comes back into the inbox as 'new' reads as
"still relevant, recheck confirmed it" when in fact the recheck never
actually completed.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import run_recheck
from hunter.store import Store
from hunter.types import Config, RunResult


@pytest.fixture
def store(tmp_path: Path) -> Store:
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    return Store(cfg)


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


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


class TestRunRecheckStaysRecheckingOnFailure:
    """A recheck that didn't reach a real verdict must stay retryable, not
    silently look like a fresh 'new' finding."""

    def test_repo_missing_stays_rechecking(self, store: Store, cfg: Config) -> None:
        """When get_repo() returns None the finding must stay 'rechecking'."""
        repo_id = store.add_repo("r", "https://example.com/r.git", "/nonexistent")
        fid, _ = store.upsert_finding(repo_id, _make_finding())
        store.set_status(fid, "rechecking")

        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["status"] == "rechecking"

        # Point the finding dict at a repo_id that doesn't exist.
        finding["repo_id"] = 9999

        result = run_recheck(store, cfg, finding)
        assert "error" in result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rechecking", (
            f"finding reset to {after['status']!r}, expected to stay 'rechecking' for retry"
        )

    def test_clone_failure_stays_rechecking(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """When git clone fails the finding must stay 'rechecking'."""
        # Path that doesn't exist yet -> triggers the clone branch.
        clone_dest = str(tmp_path / "repos" / "will-fail")
        repo_id = store.add_repo(
            "bad-clone", str(tmp_path / "nonexistent-source"), clone_dest
        )
        fid, _ = store.upsert_finding(repo_id, _make_finding(
            fingerprint="repo:f.py:fn:clone-fail",
        ))
        store.set_status(fid, "rechecking")

        finding = store.get_finding(fid)
        assert finding is not None
        result = run_recheck(store, cfg, finding)
        assert "error" in result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rechecking", (
            f"finding reset to {after['status']!r}, expected to stay 'rechecking' for retry"
        )

    def test_git_sync_failure_stays_rechecking(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """When git fetch/checkout/pull fails the finding must stay 'rechecking'."""
        # Path exists but is not a git repo -> git commands fail.
        not_a_repo = tmp_path / "repos" / "not-git"
        not_a_repo.mkdir(parents=True)
        repo_id = store.add_repo(
            "bad-sync", "https://example.com/r.git", str(not_a_repo)
        )
        fid, _ = store.upsert_finding(repo_id, _make_finding(
            fingerprint="repo:f.py:fn:sync-fail",
        ))
        store.set_status(fid, "rechecking")

        finding = store.get_finding(fid)
        assert finding is not None
        result = run_recheck(store, cfg, finding)
        assert "error" in result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rechecking", (
            f"finding reset to {after['status']!r}, expected to stay 'rechecking' for retry"
        )

    def test_killed_worker_stays_rechecking(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """When the worker is killed mid-investigation (no valid verdict
        written), the finding must stay 'rechecking' for automatic retry --
        it must NOT look like a finding that was reviewed and is still
        relevant. This is the exact scenario reported in production: a
        recheck hit its token cap, got SIGTERM'd, and silently reappeared
        as 'new' with no indication the recheck never actually completed."""

        # Minimal real git repo so fetch/checkout/pull succeed.
        repo_path = tmp_path / "repos" / "real-repo"
        repo_path.mkdir(parents=True)

        def run(*a: str) -> subprocess.CompletedProcess[bytes]:
            return subprocess.run(a, cwd=repo_path, check=True, capture_output=True)

        run("git", "init", "-b", "main")
        run("git", "config", "user.email", "t@t.com")
        run("git", "config", "user.name", "t")
        (repo_path / "f.py").write_text("pass\n")
        run("git", "add", ".")
        run("git", "commit", "-m", "init")
        run("git", "remote", "add", "origin", str(repo_path))
        run("git", "fetch", "origin")
        run("git", "branch", "--set-upstream-to=origin/main", "main")

        repo_id = store.add_repo("real-repo", str(repo_path), str(repo_path), default_branch="main")
        fid, _ = store.upsert_finding(repo_id, _make_finding(fingerprint="repo:f.py:fn:killed"))
        store.set_status(fid, "rechecking")

        finding = store.get_finding(fid)
        assert finding is not None
        finding["budget_override"] = "exempt"  # bypass "no window data" denial in tests

        def fake_run_worker(*_args: object, **_kwargs: object) -> RunResult:
            return RunResult(
                exit_code=-15,
                killed_reason="cap",
                tokens_new=200_000,
                calls=10,
                session_file=None,
                duration_s=30.0,
                stdout_tail="investigating...",
            )

        monkeypatch.setattr(scheduler.runner, "run_worker", fake_run_worker)

        result = run_recheck(store, cfg, finding)
        assert result.get("outcome") == "requeued"

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rechecking", (
            f"finding reset to {after['status']!r} after a killed worker, "
            "expected to stay 'rechecking'"
        )


class TestRecheckGiveUp:
    """The other half of the fix: a recheck that fails the SAME way over
    and over must eventually stop -- 'stays rechecking forever' is itself
    a starvation bug (see hunter.scheduler.MAX_CONSECUTIVE_SAME_FAILURE),
    since pick_next always retries the oldest 'rechecking' item first."""

    def test_gives_up_after_consecutive_identical_failures(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        repo_path = tmp_path / "repos" / "real-repo"
        repo_path.mkdir(parents=True)

        def run(*a: str) -> subprocess.CompletedProcess[bytes]:
            return subprocess.run(a, cwd=repo_path, check=True, capture_output=True)

        run("git", "init", "-b", "main")
        run("git", "config", "user.email", "t@t.com")
        run("git", "config", "user.name", "t")
        (repo_path / "f.py").write_text("pass\n")
        run("git", "add", ".")
        run("git", "commit", "-m", "init")
        run("git", "remote", "add", "origin", str(repo_path))
        run("git", "fetch", "origin")
        run("git", "branch", "--set-upstream-to=origin/main", "main")

        repo_id = store.add_repo(
            "real-repo", str(repo_path), str(repo_path), default_branch="main"
        )
        fid, _ = store.upsert_finding(repo_id, _make_finding(fingerprint="repo:f.py:fn:give-up"))
        store.set_status(fid, "rechecking")

        def fake_run_worker(*_args: object, **_kwargs: object) -> RunResult:
            return RunResult(
                exit_code=-15,
                killed_reason="cap",
                tokens_new=200_000,
                calls=10,
                session_file=None,
                duration_s=30.0,
                stdout_tail="investigating...",
            )

        monkeypatch.setattr(scheduler.runner, "run_worker", fake_run_worker)

        finding = store.get_finding(fid)
        assert finding is not None
        finding["budget_override"] = "exempt"

        for attempt in range(1, scheduler.MAX_CONSECUTIVE_SAME_FAILURE):
            result = run_recheck(store, cfg, finding)
            assert result.get("outcome") == "requeued", (attempt, result)
            after = store.get_finding(fid)
            assert after is not None
            assert after["status"] == "rechecking"
            assert after["recheck_attempts"] == attempt
            finding = after
            finding["budget_override"] = "exempt"

        result = run_recheck(store, cfg, finding)
        assert result.get("outcome") == "stuck", result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "new", (
            f"expected the finding back in the inbox as 'new' after giving up on a"
            f" stuck recheck, got {after['status']!r}"
        )
        assert after["recheck_attempts"] == 0
