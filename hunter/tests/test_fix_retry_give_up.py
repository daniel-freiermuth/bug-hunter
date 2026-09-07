"""Tests for run_fix's consecutive-same-failure give-up mechanism.

Before this fix, run_fix's salvage path unconditionally requeued a
finding on any incomplete outcome, with no attempt counter or backoff --
a finding stuck on a deterministic failure (a permanently broken push
target, a worker that always misses PR-DESCRIPTION.md, etc) would be
retried by the daemon at zero-sleep pace (server._compute_sleep_s sleeps
0s whenever the fix queue is non-empty) forever, burning real tokens on
an outcome guaranteed to repeat. Store.record_fix_attempt now tracks a
consecutive-identical-failure streak; run_fix gives up (status ->
rejected) once it crosses MAX_CONSECUTIVE_SAME_FAILURE, and a
genuinely different failure reason resets the streak instead of
accumulating it.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import MAX_CONSECUTIVE_SAME_FAILURE, run_fix
from hunter.store import Store
from hunter.types import Config, RunResult


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
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


def _run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _make_repo(tmp_path: Path) -> tuple[Path, Path]:
    seed = tmp_path / "seed"
    seed.mkdir()
    _run("git", "init", "-b", "main", cwd=seed)
    _run("git", "config", "user.email", "t@t.com", cwd=seed)
    _run("git", "config", "user.name", "t", cwd=seed)
    (seed / "f.py").write_text("pass\n")
    _run("git", "add", ".", cwd=seed)
    _run("git", "commit", "-m", "init", cwd=seed)

    upstream_path = tmp_path / "upstream.git"
    _run("git", "clone", "--bare", str(seed), str(upstream_path), cwd=tmp_path)
    repo_path = tmp_path / "repo"
    _run("git", "clone", str(upstream_path), str(repo_path), cwd=tmp_path)
    _run("git", "config", "user.email", "t@t.com", cwd=repo_path)
    _run("git", "config", "user.name", "t", cwd=repo_path)
    return upstream_path, repo_path


def _setup(store: Store, tmp_path: Path) -> dict[str, Any]:
    upstream_path, repo_path = _make_repo(tmp_path)
    repo_id = store.add_repo("repo", str(upstream_path), str(repo_path), default_branch="main")
    fid, _ = store.upsert_finding(repo_id, _make_finding())
    store.set_status(fid, "queued")
    finding = store.get_finding(fid)
    assert finding is not None
    finding["budget_override"] = "exempt"  # bypass "no window data" denial
    return finding


def _worker_that_commits_without_pr_description(
    _cfg: Config, worktree: Path, *_a: object, **_kw: object
) -> RunResult:
    """Same deterministic outcome every attempt -> failure ==
    'no PR-DESCRIPTION.md' each time, exercising the same-reason streak."""
    (worktree / "f.py").write_text("pass\nfixed\n")
    _run("git", "add", "-A", cwd=worktree)
    _run("git", "commit", "-m", "fix the bug", cwd=worktree)
    return RunResult(
        exit_code=0,
        killed_reason=None,
        tokens_new=1000,
        calls=1,
        session_file=None,
        duration_s=5.0,
        stdout_tail="done",
    )


def _worker_with_no_commits(_cfg: Config, _worktree: Path, *_a: object, **_kw: object) -> RunResult:
    return RunResult(
        exit_code=0,
        killed_reason=None,
        tokens_new=200,
        calls=1,
        session_file=None,
        duration_s=1.0,
        stdout_tail="nothing to do",
    )


class TestFixRetryGiveUp:
    def test_gives_up_after_consecutive_identical_failures(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        fid = finding["id"]
        monkeypatch.setattr(
            scheduler.runner, "run_worker", _worker_that_commits_without_pr_description
        )

        for attempt in range(1, MAX_CONSECUTIVE_SAME_FAILURE):
            result = run_fix(store, cfg, finding)
            assert result.get("outcome") == "requeued", (attempt, result)
            after = store.get_finding(fid)
            assert after is not None
            assert after["status"] == "queued"
            assert after["fix_attempts"] == attempt
            finding = after
            finding["budget_override"] = "exempt"

        # The Nth identical failure crosses the threshold -> give up rather
        # than requeue into another guaranteed-identical retry.
        result = run_fix(store, cfg, finding)
        assert result.get("outcome") == "stuck", result
        assert result.get("attempts") == MAX_CONSECUTIVE_SAME_FAILURE

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rejected"
        assert after["verdict_reason"] is not None
        assert "no PR-DESCRIPTION.md" in after["verdict_reason"]
        assert after["fix_attempts"] == 0, (
            "streak must be cleared once the finding leaves the retry loop"
        )

    def test_different_failure_reason_resets_the_streak(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        fid = finding["id"]

        monkeypatch.setattr(scheduler.runner, "run_worker", _worker_with_no_commits)
        result = run_fix(store, cfg, finding)
        assert result.get("failure") == "no commits"
        after = store.get_finding(fid)
        assert after is not None
        assert after["fix_attempts"] == 1
        finding = after
        finding["budget_override"] = "exempt"

        monkeypatch.setattr(
            scheduler.runner, "run_worker", _worker_that_commits_without_pr_description
        )
        result = run_fix(store, cfg, finding)
        assert result.get("failure") == "no PR-DESCRIPTION.md"
        after = store.get_finding(fid)
        assert after is not None
        assert after["fix_attempts"] == 1, (
            "a genuinely different failure must reset the streak, not accumulate it"
        )

    def test_success_after_failures_clears_the_streak(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        fid = finding["id"]

        monkeypatch.setattr(scheduler.runner, "run_worker", _worker_with_no_commits)
        run_fix(store, cfg, finding)
        after = store.get_finding(fid)
        assert after is not None
        assert after["fix_attempts"] == 1
        finding = after
        finding["budget_override"] = "exempt"

        class FakeForge:
            def ssh_url(self, url: str) -> str:
                return url

            def owner_repo(self, url: str) -> str:
                return "owner/repo"

            def create_pr(self, *a: object, **kw: object) -> tuple[int, str]:
                return 0, "https://example.com/pull/1"

        def _worker_that_ships(
            _cfg: Config, worktree: Path, *_a: object, **_kw: object
        ) -> RunResult:
            (worktree / "f.py").write_text("pass\nfixed\n")
            _run("git", "add", "-A", cwd=worktree)
            _run("git", "commit", "-m", "fix the bug", cwd=worktree)
            (worktree / "PR-DESCRIPTION.md").write_text("Fixes the bug.\n")
            return RunResult(
                exit_code=0,
                killed_reason=None,
                tokens_new=1000,
                calls=1,
                session_file=None,
                duration_s=5.0,
                stdout_tail="done",
            )

        monkeypatch.setattr(scheduler.runner, "run_worker", _worker_that_ships)
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: FakeForge())
        result = run_fix(store, cfg, finding)
        assert result.get("outcome") == "pr_open", result

        after = store.get_finding(fid)
        assert after is not None
        assert after["fix_attempts"] == 0
        assert after["last_fix_failure"] is None
