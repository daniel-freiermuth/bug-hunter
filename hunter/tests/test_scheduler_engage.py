"""Tests for run_engage — the PR engagement lifecycle in scheduler.py."""

from __future__ import annotations

from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from hunter.scheduler import run_engage
from hunter.store import Store
from hunter.types import BudgetDecision, Config, RunResult


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


def _seed(
    store: Store,
    *,
    repo_url: str = "https://github.com/owner/repo",
    repo_path: str = "/fake/repo",
    pr_number: int = 42,
    head_ref: str = "fix-branch",
    budget_override: str | None = None,
    finding_overrides: dict[str, Any] | None = None,
) -> tuple[int, int, dict[str, Any]]:
    """Insert a repo + finding + pr_state and return (repo_id, fid, finding)."""
    rid = store.add_repo("r", repo_url, repo_path)
    fid, _ = store.upsert_finding(rid, _make_finding(**(finding_overrides or {})))
    store.upsert_pr_state(fid, pr_number=pr_number, head_ref=head_ref)
    if budget_override:
        store.set_budget_override(fid, budget_override)
    finding = store.get_finding(fid)
    assert finding is not None
    return rid, fid, finding


def _ok_run_result() -> RunResult:
    return RunResult(
        exit_code=0,
        killed_reason=None,
        tokens_new=100,
        calls=2,
        session_file=None,
        duration_s=5.0,
        stdout_tail="done",
    )


def _failed_run_result() -> RunResult:
    return RunResult(
        exit_code=1,
        killed_reason=None,
        tokens_new=50,
        calls=1,
        session_file=None,
        duration_s=3.0,
        stdout_tail="error output",
    )


def _make_run_cmd(
    *,
    log_output: str = "",
    push_rc: int = 0,
    push_output: str = "",
) -> Any:
    """Return a run_cmd side-effect that creates the worktree dir on 'worktree add'."""

    def _side_effect(cmd: list[str], timeout: int = 120) -> tuple[int, str]:
        # `git worktree add --detach <path> ...` — create the directory.
        if "worktree" in cmd and "add" in cmd:
            idx = cmd.index("add")
            # path is two positions after 'add' (skips --detach)
            for i in range(idx + 1, len(cmd)):
                if not cmd[i].startswith("-"):
                    Path(cmd[i]).mkdir(parents=True, exist_ok=True)
                    break
            return (0, "")
        if "log" in cmd:
            return (0, log_output)
        if "push" in cmd:
            return (push_rc, push_output)
        return (0, "")

    return _side_effect


# ---------------------------------------------------------------------------
# State-guard early returns (lines 808–831)
# ---------------------------------------------------------------------------


class TestStateGuards:
    """Early-return validation paths at the top of run_engage."""

    def test_missing_repo_returns_error(self, store: Store, cfg: Config) -> None:
        """get_repo returns None → error dict, no crash."""
        rid = store.add_repo("r", "https://github.com/o/r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        finding = store.get_finding(fid)
        assert finding is not None
        # Point at a repo_id that doesn't exist.
        finding["repo_id"] = 9999

        result = run_engage(store, cfg, finding)

        assert result == {"error": "repo missing"}

    def test_missing_pr_state_returns_error(self, store: Store, cfg: Config) -> None:
        """No pr_state row → error dict."""
        rid = store.add_repo("r", "https://github.com/o/r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        finding = store.get_finding(fid)
        assert finding is not None
        # No upsert_pr_state call → get_pr_state returns None.

        result = run_engage(store, cfg, finding)

        assert result == {"error": "no pr_state"}

    def test_pr_state_missing_head_ref_returns_error(
        self, store: Store, cfg: Config
    ) -> None:
        """pr_state present but head_ref missing → error."""
        rid = store.add_repo("r", "https://github.com/o/r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.upsert_pr_state(fid, pr_number=42)
        finding = store.get_finding(fid)
        assert finding is not None

        result = run_engage(store, cfg, finding)

        assert result == {"error": "no pr_state"}

    def test_unparseable_repo_url_returns_error(
        self, store: Store, cfg: Config
    ) -> None:
        """owner_repo returns None for a garbage URL → error."""
        rid = store.add_repo("r", "not-a-valid-url", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.upsert_pr_state(fid, pr_number=42, head_ref="branch")
        finding = store.get_finding(fid)
        assert finding is not None

        result = run_engage(store, cfg, finding)

        assert result == {"error": "unparseable repo url"}


# ---------------------------------------------------------------------------
# Budget denial (line 925)
# ---------------------------------------------------------------------------


class TestBudgetDenial:
    """Budget deny path drops the worktree and records the denial."""

    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch("hunter.scheduler.run_cmd", return_value=(0, ""))
    @patch("hunter.scheduler.forge_for")
    def test_budget_denied(
        self,
        mock_forge_for: MagicMock,
        mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        _mock_runner: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(False, "over budget", 0)

        _, fid, finding = _seed(store)
        result = run_engage(store, cfg, finding)

        assert "denied" in result
        assert result["denied"] == "over budget"
        # A job must have been created with state=denied.
        jobs = store.list_jobs()
        assert len(jobs) == 1
        assert jobs[0]["state"] == "denied"


# ---------------------------------------------------------------------------
# WITHDRAW path (line 973–994)
# ---------------------------------------------------------------------------


class TestWithdraw:
    """Worker writes WITHDRAW.md → PR closed, finding rejected, worktree removed."""

    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch("hunter.scheduler.run_cmd", side_effect=_make_run_cmd())
    @patch("hunter.scheduler.forge_for")
    def test_withdraw_closes_pr_and_rejects(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix", "body": "b"}, "")
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)

        def _run_worker_side_effect(
            _cfg: Any, worktree: Path, *args: Any, **kwargs: Any
        ) -> RunResult:
            (worktree / "WITHDRAW.md").write_text("Not fixable: design issue")
            return _ok_run_result()

        mock_runner.side_effect = _run_worker_side_effect

        _, fid, finding = _seed(store)
        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "withdrawn"
        # Forge must have been told to close the PR.
        forge.close_pr.assert_called_once()
        slug, num, comment = forge.close_pr.call_args[0]
        assert slug == "owner/repo"
        assert num == 42
        assert "Not fixable" in comment
        # Finding status must be rejected.
        after = store.get_finding(fid)
        assert after is not None
        assert after["status"] == "rejected"


# ---------------------------------------------------------------------------
# Push + comment success path (line 996–1074)
# ---------------------------------------------------------------------------


class TestPushAndComment:
    """Worker pushes commits and posts a PR reply → engaged with watermark +3s."""

    @patch("hunter.scheduler.now_ms")
    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch("hunter.scheduler.run_cmd", side_effect=_make_run_cmd(log_output="abc1234 commit msg"))
    @patch("hunter.scheduler.forge_for")
    def test_push_and_reply_watermark(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        mock_now: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        fixed_now = 1_000_000
        mock_now.return_value = fixed_now

        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix"}, "")
        forge.comment_pr.return_value = (0, "ok")
        forge.ssh_url.return_value = "git@github.com:owner/repo.git"
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)

        def _run_worker_side_effect(
            _cfg: Any, worktree: Path, *args: Any, **kwargs: Any
        ) -> RunResult:
            (worktree / "PR-REPLY.md").write_text("Here is the fix")
            return _ok_run_result()

        mock_runner.side_effect = _run_worker_side_effect

        _, fid, finding = _seed(store)
        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "engaged"
        assert result["pushed"] is True
        assert result["replied"] is True
        # Watermark must be now_ms() + 3_000 when replied.
        ps = store.get_pr_state(fid)
        assert ps is not None
        assert ps["last_engaged_activity_at"] == fixed_now + 3_000


class TestNoOp:
    """No new commits and no reply file → 'no-op' in the did list."""

    @patch("hunter.scheduler.now_ms", return_value=1_000_000)
    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch("hunter.scheduler.run_cmd", side_effect=_make_run_cmd())
    @patch("hunter.scheduler.forge_for")
    def test_no_commits_no_reply(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        _mock_now: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix"}, "")
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)
        mock_runner.return_value = _ok_run_result()

        _, fid, finding = _seed(store)
        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "engaged"
        assert result["pushed"] is False
        assert result["replied"] is False


# ---------------------------------------------------------------------------
# Push / comment failure (lines 1036–1051)
# ---------------------------------------------------------------------------


class TestPushFailure:
    """Push fails → failure logged, worktree kept, outcome='retry'."""

    @patch("hunter.scheduler.now_ms", return_value=1_000_000)
    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch(
        "hunter.scheduler.run_cmd",
        side_effect=_make_run_cmd(
            log_output="abc1234 commit msg", push_rc=1, push_output="push rejected"
        ),
    )
    @patch("hunter.scheduler.forge_for")
    def test_push_fails_keeps_worktree(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        _mock_now: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix"}, "")
        forge.ssh_url.return_value = "git@github.com:owner/repo.git"
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)
        mock_runner.return_value = _ok_run_result()

        _, fid, finding = _seed(store)
        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "retry"
        assert "push failed" in result["failure"]
        # Job state should have been flipped to failed.
        jobs = store.list_jobs()
        running_jobs = [j for j in jobs if j["finding_id"] == fid and j["state"] == "failed"]
        assert len(running_jobs) == 1


class TestCommentFailure:
    """Comment fails → failure logged, worktree kept, outcome='retry'."""

    @patch("hunter.scheduler.now_ms", return_value=1_000_000)
    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch("hunter.scheduler.run_cmd", side_effect=_make_run_cmd())
    @patch("hunter.scheduler.forge_for")
    def test_comment_fails_keeps_worktree(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        _mock_now: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix"}, "")
        forge.ssh_url.return_value = "git@github.com:owner/repo.git"
        forge.comment_pr.return_value = (1, "comment error")
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)

        def _run_worker_side_effect(
            _cfg: Any, worktree: Path, *args: Any, **kwargs: Any
        ) -> RunResult:
            (worktree / "PR-REPLY.md").write_text("Here is the fix")
            return _ok_run_result()

        mock_runner.side_effect = _run_worker_side_effect

        _, fid, finding = _seed(store)
        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "retry"
        assert "PR comment failed" in result["failure"]


# ---------------------------------------------------------------------------
# Budget override 'once' cleared after success and failure (lines 1049, 1072)
# ---------------------------------------------------------------------------


class TestBudgetOverrideOnce:
    """budget_override='once' is cleared after both success and failure paths."""

    @patch("hunter.scheduler.now_ms", return_value=1_000_000)
    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch("hunter.scheduler.run_cmd", side_effect=_make_run_cmd())
    @patch("hunter.scheduler.forge_for")
    def test_override_cleared_on_success(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        _mock_now: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix"}, "")
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)
        mock_runner.return_value = _ok_run_result()

        _, fid, finding = _seed(store, budget_override="once")
        assert finding.get("budget_override") == "once"

        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "engaged"
        after = store.get_finding(fid)
        assert after is not None
        assert after["budget_override"] is None

    @patch("hunter.scheduler.now_ms", return_value=1_000_000)
    @patch("hunter.scheduler.runner.run_worker")
    @patch("hunter.scheduler.build_engage_prompt", return_value="prompt")
    @patch("hunter.scheduler.budget.decide")
    @patch("hunter.scheduler.budget.read_windows", return_value={})
    @patch(
        "hunter.scheduler.run_cmd",
        side_effect=_make_run_cmd(
            log_output="abc1234 commit msg", push_rc=1, push_output="push rejected"
        ),
    )
    @patch("hunter.scheduler.forge_for")
    def test_override_cleared_on_failure(
        self,
        mock_forge_for: MagicMock,
        _mock_run_cmd: MagicMock,
        _mock_windows: MagicMock,
        mock_decide: MagicMock,
        _mock_prompt: MagicMock,
        mock_runner: MagicMock,
        _mock_now: MagicMock,
        store: Store,
        cfg: Config,
    ) -> None:
        forge = MagicMock()
        forge.owner_repo.return_value = "owner/repo"
        forge.view_pr_engage.return_value = (0, {"title": "Fix"}, "")
        forge.ssh_url.return_value = "git@github.com:owner/repo.git"
        mock_forge_for.return_value = forge
        mock_decide.return_value = BudgetDecision(True, "ok", 150_000)
        mock_runner.return_value = _ok_run_result()

        _, fid, finding = _seed(store, budget_override="once")

        result = run_engage(store, cfg, finding)

        assert result["outcome"] == "retry"
        after = store.get_finding(fid)
        assert after is not None
        assert after["budget_override"] is None
