"""Tests for run_fix -- worktree lifecycle, PR creation, NOT-A-BUG, salvage."""

from __future__ import annotations

from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from hunter.scheduler import run_fix
from hunter.store import Store
from hunter.types import BudgetDecision, Config, RunResult


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


def _ok_run_result(**overrides: Any) -> RunResult:
    """A successful worker run."""
    defaults: dict[str, Any] = {
        "exit_code": 0,
        "killed_reason": None,
        "tokens_new": 500,
        "calls": 3,
        "session_file": None,
        "duration_s": 10.0,
        "stdout_tail": "all done",
    }
    defaults.update(overrides)
    return RunResult(**defaults)


def _failed_run_result(**overrides: Any) -> RunResult:
    """A failed worker run."""
    defaults: dict[str, Any] = {
        "exit_code": 1,
        "killed_reason": None,
        "tokens_new": 200,
        "calls": 1,
        "session_file": None,
        "duration_s": 5.0,
        "stdout_tail": "error output",
    }
    defaults.update(overrides)
    return RunResult(**defaults)


def _insert_finding(
    store: Store, *, status: str = "queued", budget_override: str | None = None,
) -> tuple[int, int]:
    """Insert a repo + queued finding; return (repo_id, finding_id)."""
    rid = store.add_repo("r", "https://example.com/r.git", "/repo")
    fid, _ = store.upsert_finding(rid, _make_finding())
    store.set_status(fid, status)
    if budget_override:
        store.set_budget_override(fid, budget_override)
    return rid, fid


def _allow_budget() -> BudgetDecision:
    return BudgetDecision(allow=True, reason="ok", cap_tokens=100_000)


def _deny_budget() -> BudgetDecision:
    return BudgetDecision(allow=False, reason="over budget")


def _make_forge_mock(
    *,
    create_pr_rc: int = 0,
    create_pr_out: str = "https://github.com/o/r/pull/42",
) -> MagicMock:
    forge = MagicMock()
    forge.ssh_url.return_value = "git@github.com:o/r.git"
    forge.owner_repo.return_value = "o/r"
    forge.create_pr.return_value = (create_pr_rc, create_pr_out)
    return forge


# Patches applied to every test that reaches the worker.
_SCHED = "hunter.scheduler"


class TestRunFixStateGuard:
    """Finding not 'queued' returns skipped immediately."""

    def test_fixing_status_skipped(self, store: Store, cfg: Config) -> None:
        _, fid = _insert_finding(store, status="fixing")
        finding = store.get_finding(fid)
        assert finding is not None

        result = run_fix(store, cfg, finding)
        assert "skipped" in result
        assert "fixing" in result["skipped"]

    def test_rejected_status_skipped(self, store: Store, cfg: Config) -> None:
        _, fid = _insert_finding(store, status="rejected")
        finding = store.get_finding(fid)
        assert finding is not None

        result = run_fix(store, cfg, finding)
        assert "skipped" in result

    def test_repo_missing_returns_error(self, store: Store, cfg: Config) -> None:
        _, fid = _insert_finding(store, status="queued")
        finding = store.get_finding(fid)
        assert finding is not None
        finding["repo_id"] = 9999

        result = run_fix(store, cfg, finding)
        assert "error" in result
        assert "repo missing" in result["error"]


class TestRunFixWorktreeReclaim:
    """Stale worktree from a prior attempt is removed and recreated."""

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_stale_worktree_removed_before_add(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
        tmp_path: Path,
    ) -> None:
        mock_decide.return_value = _allow_budget()
        mock_worker.return_value = _failed_run_result()
        mock_forge_for.return_value = _make_forge_mock()

        rid, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None

        # Pre-create the worktree directory to simulate a stale worktree.
        wt_dir = cfg.work_root / "wt" / f"f{fid}"
        wt_dir.mkdir(parents=True)

        # run_cmd calls: remove worktree, delete branch, add worktree, then
        # log commits (and potentially more).  We just need add-worktree
        # to succeed.
        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                # Create the directory so Path.exists() works downstream.
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd:
                return 0, ""
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect
        run_fix(store, cfg, finding)

        # Verify that `worktree remove --force` was called (reclaim).
        calls = [c.args[0] for c in mock_run_cmd.call_args_list]
        remove_calls = [c for c in calls if "worktree" in c and "remove" in c]
        assert len(remove_calls) >= 1, "expected worktree remove for reclaim"

        # And branch -D was called (cleanup stale branch).
        branch_d_calls = [c for c in calls if "branch" in c and "-D" in c]
        assert len(branch_d_calls) >= 1, "expected branch -D for reclaim"


class TestRunFixNotABug:
    """Worker creates NOT-A-BUG.md -> finding rejected, worktree cleaned."""

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_not_a_bug_rejects_finding(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        mock_decide.return_value = _allow_budget()
        mock_forge_for.return_value = _make_forge_mock()

        rid, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None

        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect

        # Worker succeeds but creates NOT-A-BUG.md.
        def run_worker_side_effect(*_a: Any, **_kw: Any) -> RunResult:
            (wt_dir / "NOT-A-BUG.md").write_text("This is not a real bug.")
            return _ok_run_result()

        mock_worker.side_effect = run_worker_side_effect

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "rejected"

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rejected"
        assert "not a real bug" in (after["verdict_reason"] or "").lower()

        # Worktree drop was called (delete_branch=True).
        calls = [c.args[0] for c in mock_run_cmd.call_args_list]
        post_worker_removes = [
            c for c in calls if "worktree" in c and "remove" in c
        ]
        assert len(post_worker_removes) >= 1


class TestRunFixPRCreation:
    """Commits + PR-DESCRIPTION.md -> push + forge.create_pr -> pr_open."""

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_successful_pr_creation(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        mock_decide.return_value = _allow_budget()
        forge = _make_forge_mock()
        mock_forge_for.return_value = forge

        rid, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None

        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd and "--oneline" in cmd:
                return 0, "abc1234 fix: the bug"
            if "push" in cmd:
                return 0, ""
            if "log" in cmd and "--format=%s" in cmd:
                return 0, "fix: the bug"
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect

        def run_worker_side_effect(*_a: Any, **_kw: Any) -> RunResult:
            (wt_dir / "PR-DESCRIPTION.md").write_text("Fixed the bug.")
            return _ok_run_result()

        mock_worker.side_effect = run_worker_side_effect

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "pr_open"
        assert result.get("pr_url") == "https://github.com/o/r/pull/42"

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "pr_open"
        assert after["pr_url"] == "https://github.com/o/r/pull/42"

        # forge.create_pr was called.
        forge.create_pr.assert_called_once()


class TestRunFixAlreadyExists:
    """create_pr fails with 'already exists' -- recovery depends on URL format."""

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_github_already_exists_recovers(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        mock_decide.return_value = _allow_budget()
        forge = _make_forge_mock(
            create_pr_rc=1,
            create_pr_out=(
                "a]pull request already exists: "
                "https://github.com/o/r/pull/99"
            ),
        )
        mock_forge_for.return_value = forge

        _, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None
        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd and "--oneline" in cmd:
                return 0, "abc1234 fix: it"
            if "push" in cmd:
                return 0, ""
            if "log" in cmd and "--format=%s" in cmd:
                return 0, "fix: it"
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect

        def run_worker_side_effect(*_a: Any, **_kw: Any) -> RunResult:
            (wt_dir / "PR-DESCRIPTION.md").write_text("Fixed.")
            return _ok_run_result()

        mock_worker.side_effect = run_worker_side_effect

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "pr_open"
        assert result.get("pr_url") == "https://github.com/o/r/pull/99"

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_gitlab_already_exists_fails_known_bug(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        """GitLab MR URL doesn't match /pull/\\d+ regex -- falls to salvage."""
        mock_decide.return_value = _allow_budget()
        forge = _make_forge_mock(
            create_pr_rc=1,
            create_pr_out=(
                "a]merge request already exists: "
                "https://gitlab.com/o/r/-/merge_requests/7"
            ),
        )
        mock_forge_for.return_value = forge

        _, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None
        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd and "--oneline" in cmd:
                return 0, "abc1234 fix: it"
            if "push" in cmd:
                return 0, ""
            if "log" in cmd and "--format=%s" in cmd:
                return 0, "fix: it"
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect

        def run_worker_side_effect(*_a: Any, **_kw: Any) -> RunResult:
            (wt_dir / "PR-DESCRIPTION.md").write_text("Fixed.")
            return _ok_run_result()

        mock_worker.side_effect = run_worker_side_effect

        result = run_fix(store, cfg, finding)

        # Known bug: GitLab URL not matched → salvage/requeue.
        assert result.get("outcome") == "requeued"
        assert "PR create failed" in result.get("failure", "")

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "queued"


class TestRunFixSalvage:
    """Worker failure -> finding requeued, worktree kept."""

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_worker_failure_requeues(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        mock_decide.return_value = _allow_budget()
        mock_forge_for.return_value = _make_forge_mock()

        _, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None
        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd:
                return 0, ""  # no commits
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect
        mock_worker.return_value = _failed_run_result()

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "requeued"
        assert "failure" in result

        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "queued"

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.decide")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_no_commits_requeues(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        """Worker succeeds but makes no commits -> salvage."""
        mock_decide.return_value = _allow_budget()
        mock_forge_for.return_value = _make_forge_mock()

        _, fid = _insert_finding(store)
        finding = store.get_finding(fid)
        assert finding is not None
        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd and "--oneline" in cmd:
                return 0, ""  # no commits
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect
        mock_worker.return_value = _ok_run_result()

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "requeued"
        assert "no commits" in result.get("failure", "")


class TestRunFixBudgetOverride:
    """'once' override cleared on completion and on failure."""

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_once_override_cleared_on_pr_open(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = _make_forge_mock()
        mock_forge_for.return_value = forge

        _, fid = _insert_finding(store, budget_override="once")
        finding = store.get_finding(fid)
        assert finding is not None
        assert finding["budget_override"] == "once"

        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd and "--oneline" in cmd:
                return 0, "abc1234 fix"
            if "push" in cmd:
                return 0, ""
            if "log" in cmd and "--format=%s" in cmd:
                return 0, "fix"
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect

        def run_worker_side_effect(*_a: Any, **_kw: Any) -> RunResult:
            (wt_dir / "PR-DESCRIPTION.md").write_text("Fixed.")
            return _ok_run_result()

        mock_worker.side_effect = run_worker_side_effect

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "pr_open"

        after = store.get_finding(fid)
        assert after is not None
        assert after["budget_override"] is None, (
            "'once' override must be cleared after successful PR"
        )

    @patch(f"{_SCHED}.runner.run_worker")
    @patch(f"{_SCHED}.build_fix_prompt", return_value="prompt")
    @patch(f"{_SCHED}.budget.read_windows", return_value={})
    @patch(f"{_SCHED}.run_cmd")
    @patch(f"{_SCHED}.forge_for")
    def test_once_override_cleared_on_salvage(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        _mock_prompt: MagicMock,
        mock_worker: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        mock_forge_for.return_value = _make_forge_mock()

        _, fid = _insert_finding(store, budget_override="once")
        finding = store.get_finding(fid)
        assert finding is not None

        wt_dir = cfg.work_root / "wt" / f"f{fid}"

        def cmd_side_effect(cmd: list[str], **_kw: Any) -> tuple[int, str]:
            if "worktree" in cmd and "add" in cmd:
                wt_dir.mkdir(parents=True, exist_ok=True)
                return 0, ""
            if "log" in cmd:
                return 0, ""
            return 0, ""

        mock_run_cmd.side_effect = cmd_side_effect
        mock_worker.return_value = _failed_run_result()

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "requeued"

        after = store.get_finding(fid)
        assert after is not None
        assert after["budget_override"] is None, (
            "'once' override must be cleared after salvage too"
        )
