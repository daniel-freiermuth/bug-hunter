"""Tests for hunter.server._describe_cycle -- classifies run_cycle's
return shape into (state, detail) for the Status page's "what's
happening" panel. Must agree with daemon()'s own sleep-computation
branches on the same summary dict, since the displayed reason is
explaining that exact sleep decision."""

from __future__ import annotations

from hunter.server import _describe_cycle


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
        state, detail = _describe_cycle({"denied": "5h window exhausted", "job": 3})
        assert state == "denied"
        assert detail == "5h window exhausted"

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
