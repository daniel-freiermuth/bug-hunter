"""Regression: run_recheck must reset status to 'new' on error paths."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from hunter.scheduler import run_recheck
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


class TestRunRecheckResetsStatus:
    """Finding must not stay stuck in 'rechecking' when run_recheck fails."""

    def test_repo_missing_resets_to_new(self, store: Store, cfg: Config) -> None:
        """When get_repo() returns None the finding must go back to 'new'."""
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
        assert after["status"] == "new", (
            f"finding stuck in {after['status']!r}, expected 'new'"
        )
