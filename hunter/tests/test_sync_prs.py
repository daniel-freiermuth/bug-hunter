"""Tests for hunter.scheduler.sync_prs -- specifically attention_since
tracking, half of the fix for a real production starvation bug (see
run_engage's ENGAGE_BACKOFF_MS docstring for the other half).

Root cause: list_attention() used to order by pr_state.synced_at, which
sync_prs bulk-refreshes for EVERY pr_open finding EVERY cycle in a fixed
id-DESC iteration order -- so it reflected loop iteration order, not
genuine wait time. Fix: attention_since, which sync_prs sets ONLY when
the attention reason string actually changes, preserving it otherwise so
it keeps meaning "since when has THIS reason been outstanding".
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import sync_prs
from hunter.store import Store
from hunter.types import Config


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


class _FakeForge:
    """Returns whatever PR shape the test currently has queued; records
    nothing, does no network I/O."""

    def __init__(self, pr: dict[str, Any]) -> None:
        self.pr = pr

    def parse_pr_url(self, url: str) -> tuple[str, int] | None:
        return "owner/repo", 1

    def view_pr_sync(self, slug: str, number: int, timeout: int = 30) -> Any:
        return 0, self.pr, ""


def _pr(
    *,
    checks_failing: bool = False,
    conflicting: bool = False,
    state: str = "OPEN",
) -> dict[str, Any]:
    rollup = [{"name": "CI", "conclusion": "FAILURE"}] if checks_failing else []
    return {
        "state": state,
        "comments": [],
        "reviews": [],
        "reviewDecision": None,
        "mergeable": "CONFLICTING" if conflicting else "MERGEABLE",
        "statusCheckRollup": rollup,
        "updatedAt": "2026-01-01T00:00:00Z",
        "headRefName": "feature",
    }


def _setup(store: Store, tmp_path: Path) -> dict[str, Any]:
    rid = store.add_repo("r", "https://r", "/nonexistent")
    fid, _ = store.upsert_finding(rid, _make_finding())
    store.set_status(fid, "pr_open")
    finding = store.get_finding(fid)
    assert finding is not None
    return finding


class TestAttentionSinceTracking:
    def test_first_flag_sets_attention_since(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        monkeypatch.setattr(
            scheduler, "forge_for", lambda repo: _FakeForge(_pr(checks_failing=True))
        )

        sync_prs(store, cfg)

        ps = store.get_pr_state(finding["id"])
        assert ps is not None
        assert ps["needs_attention"] == "checks_failing"
        assert ps["attention_since"] is not None

    def test_same_reason_preserves_attention_since_across_syncs(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The regression this whole mechanism exists to prevent: without
        this, attention_since (and the old synced_at it replaced) would
        get bumped to "now" on every single cycle even though nothing
        about the situation changed, making a PR that's been stuck for
        an hour look exactly as fresh as one flagged a second ago."""
        finding = _setup(store, tmp_path)
        monkeypatch.setattr(
            scheduler, "forge_for", lambda repo: _FakeForge(_pr(checks_failing=True))
        )

        sync_prs(store, cfg)
        first = store.get_pr_state(finding["id"])
        assert first is not None
        first_since = first["attention_since"]

        sync_prs(store, cfg)  # identical PR state, still checks_failing
        second = store.get_pr_state(finding["id"])
        assert second is not None
        assert second["attention_since"] == first_since

    def test_reason_change_resets_attention_since(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        forge_box = {"forge": _FakeForge(_pr(checks_failing=True))}
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: forge_box["forge"])

        sync_prs(store, cfg)
        first = store.get_pr_state(finding["id"])
        assert first is not None
        assert first["needs_attention"] == "checks_failing"
        first_since = first["attention_since"]

        # A genuinely different situation: checks now pass but there's a
        # merge conflict instead. Sleep to guarantee now_ms() actually
        # advances a tick -- this test's real assertion is "the value
        # changed", not "enough wall-clock time passed" for its own sake.
        time.sleep(0.01)
        forge_box["forge"] = _FakeForge(_pr(checks_failing=False, conflicting=True))
        sync_prs(store, cfg)
        second = store.get_pr_state(finding["id"])
        assert second is not None
        assert second["needs_attention"] == "conflict"
        assert second["attention_since"] != first_since
        assert second["attention_since"] is not None

    def test_reason_change_clears_a_stale_backoff(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A backoff set for the OLD (now-resolved) reason must not
        suppress attention for a genuinely NEW reason that just appeared
        -- new activity is never held hostage by an unrelated backoff."""
        finding = _setup(store, tmp_path)
        from hunter.types import now_ms

        store.upsert_pr_state(
            finding["id"],
            needs_attention="checks_failing",
            attention_backoff_until=now_ms() + 999_999,
        )
        monkeypatch.setattr(
            scheduler, "forge_for", lambda repo: _FakeForge(_pr(conflicting=True))
        )

        sync_prs(store, cfg)

        ps = store.get_pr_state(finding["id"])
        assert ps is not None
        assert ps["needs_attention"] == "conflict"
        assert ps["attention_backoff_until"] is None
        attn = store.list_attention()
        assert len(attn) == 1
        assert attn[0]["id"] == finding["id"]

    def test_resolved_reason_clears_attention_since(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        forge_box = {"forge": _FakeForge(_pr(checks_failing=True))}
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: forge_box["forge"])

        sync_prs(store, cfg)
        forge_box["forge"] = _FakeForge(_pr())  # clean PR, nothing outstanding
        sync_prs(store, cfg)

        ps = store.get_pr_state(finding["id"])
        assert ps is not None
        assert ps["needs_attention"] is None
        assert ps["attention_since"] is None
