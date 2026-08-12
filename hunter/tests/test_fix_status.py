"""Regression: run_fix must reset status to 'new' on early error paths.

Mirrors test_recheck_status.py — the same bug class fixed in cfa5d31 for
run_recheck, but in run_fix: when get_repo() returns None or worktree-add
fails, the finding stays 'queued' forever, blocking every subsequent fix
attempt since run_cycle always picks the oldest queued finding.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from hunter.scheduler import run_fix
from hunter.store import Store
from hunter.types import Config


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


class TestRunFixResetsStatus:
    """Finding must not stay stuck in 'queued' when run_fix hits an error."""

    def test_repo_missing_resets_to_new(self, store: Store, cfg: Config) -> None:
        """When get_repo() returns None the finding must go back to 'new'."""
        repo_id = store.add_repo("r", "https://example.com/r.git", "/nonexistent")
        fid, _ = store.upsert_finding(repo_id, _make_finding())
        store.set_status(fid, "queued")

        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["status"] == "queued"

        # Point the finding dict at a repo_id that doesn't exist.
        finding["repo_id"] = 9999

        result = run_fix(store, cfg, finding)
        assert "error" in result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "new", (
            f"finding stuck in {after['status']!r}, expected 'new'"
        )

    def test_worktree_add_failure_resets_to_new(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """When worktree add fails the finding must go back to 'new'."""
        # repo path exists but is not a git repo → worktree add fails.
        not_a_repo = tmp_path / "repos" / "not-git"
        not_a_repo.mkdir(parents=True)
        repo_id = store.add_repo(
            "bad-wt", "https://example.com/r.git", str(not_a_repo)
        )
        fid, _ = store.upsert_finding(
            repo_id, _make_finding(fingerprint="repo:f.py:fn:wt-fail")
        )
        store.set_status(fid, "queued")

        finding = store.get_finding(fid)
        assert finding is not None
        result = run_fix(store, cfg, finding)
        assert "error" in result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "new", (
            f"finding stuck in {after['status']!r}, expected 'new'"
        )
