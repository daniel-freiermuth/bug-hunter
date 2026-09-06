"""run_engage may freely rewrite the PR's own branch history (rebase,
squash, force-push, amend) -- that is expected/desired: PR branches are
cleaned up before merging to the default branch. The only hard line is the
default branch itself, which run_engage must never target.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import run_engage
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


class _FakeForge:
    """Minimal Forge stand-in: no network, records posted comments."""

    def __init__(self) -> None:
        self.comments: list[str] = []

    def owner_repo(self, url: str) -> str:
        return "owner/repo"

    def ssh_url(self, https_url: str) -> str:
        return https_url

    def view_pr_engage(self, slug: str, number: int, timeout: int = 30):
        return 0, {"title": "t", "body": "b"}, ""

    def comment_pr(self, slug: str, number: int, body_file: Path, timeout: int = 60):
        self.comments.append(Path(body_file).read_text())
        return 0, ""


def _run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _make_repo_with_published_branch(tmp_path: Path) -> tuple[Path, Path]:
    """A real upstream (bare) + a local clone with `origin` pointing at it,
    `feature` one commit ahead of `main`. Mirrors production topology where
    repo["path"] (local clone) and repo["url"] (the actual remote) are
    genuinely distinct repositories -- unlike a self-referential remote,
    this lets us check what actually reached the remote vs. what a worker
    merely mutated in its own local branch ref."""
    seed = tmp_path / "seed"
    seed.mkdir()
    _run("git", "init", "-b", "main", cwd=seed)
    _run("git", "config", "user.email", "t@t.com", cwd=seed)
    _run("git", "config", "user.name", "t", cwd=seed)
    (seed / "f.py").write_text("pass\n")
    _run("git", "add", ".", cwd=seed)
    _run("git", "commit", "-m", "init", cwd=seed)
    _run("git", "checkout", "-b", "feature", cwd=seed)
    (seed / "f.py").write_text("pass\npublished\n")
    _run("git", "add", ".", cwd=seed)
    _run("git", "commit", "-m", "first published commit", cwd=seed)
    _run("git", "checkout", "main", cwd=seed)

    upstream_path = tmp_path / "upstream.git"
    _run("git", "clone", "--bare", str(seed), str(upstream_path), cwd=tmp_path)

    repo_path = tmp_path / "repo"
    _run("git", "clone", str(upstream_path), str(repo_path), cwd=tmp_path)
    _run("git", "config", "user.email", "t@t.com", cwd=repo_path)
    _run("git", "config", "user.name", "t", cwd=repo_path)
    return upstream_path, repo_path


def _setup(
    store: Store, tmp_path: Path, head_ref: str = "feature"
) -> tuple[dict[str, Any], _FakeForge]:
    upstream_path, repo_path = _make_repo_with_published_branch(tmp_path)
    repo_id = store.add_repo(
        "repo", str(upstream_path), str(repo_path), default_branch="main"
    )
    fid, _ = store.upsert_finding(repo_id, _make_finding())
    store.set_status(fid, "pr_open")
    store.upsert_pr_state(
        fid,
        pr_number=1,
        head_ref=head_ref,
        needs_attention="new_comments",
        synced_at=0,
    )
    finding = store.get_finding(fid)
    assert finding is not None
    finding["budget_override"] = "exempt"  # bypass "no window data" denial
    fake_forge = _FakeForge()
    return finding, fake_forge


def _fake_result() -> RunResult:
    return RunResult(
        exit_code=0,
        killed_reason=None,
        tokens_new=1000,
        calls=1,
        session_file=None,
        duration_s=5.0,
        stdout_tail="done",
    )


class TestEngageAllowsBranchHistoryRewrite:
    def test_new_commit_is_pushed(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worker that only adds commits on top of the published tip must
        have its work pushed and its reply posted."""
        finding, fake_forge = _setup(store, tmp_path)
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        def fake_run_worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
            (worktree / "f.py").write_text("pass\npublished\naddressed feedback\n")
            _run("git", "add", "-A", cwd=worktree)
            _run("git", "commit", "-m", "address feedback", cwd=worktree)
            (worktree / "PR-REPLY.md").write_text("Addressed the feedback.\n")
            return _fake_result()

        monkeypatch.setattr(scheduler.runner, "run_worker", fake_run_worker)

        result = run_engage(store, cfg, finding)
        assert result.get("outcome") == "engaged", result
        assert result.get("pushed") is True

        upstream_path = Path(store.get_repo(finding["repo_id"])["url"])
        rc = subprocess.run(
            ["git", "log", "--oneline", "feature"],
            cwd=upstream_path,
            capture_output=True,
            text=True,
            check=True,
        )
        assert "address feedback" in rc.stdout
        assert any("Addressed the feedback." in c for c in fake_forge.comments)

    def test_rebased_history_is_pushed(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worker that rewrites its own PR branch (e.g. squashing/rebasing
        to clean up commits before merge, as requested by a reviewer) must
        have the rewrite actually reach the remote -- this is desired
        behavior, not a violation."""
        finding, fake_forge = _setup(store, tmp_path)
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        def fake_run_worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
            # Drop the published commit and replace it with a cleaned-up one.
            _run("git", "reset", "--hard", "HEAD~1", cwd=worktree)
            (worktree / "f.py").write_text("pass\nsquashed and cleaned\n")
            _run("git", "add", "-A", cwd=worktree)
            _run("git", "commit", "-m", "squashed: clean commit for merge", cwd=worktree)
            (worktree / "PR-REPLY.md").write_text("Rebased and squashed as requested.\n")
            return _fake_result()

        monkeypatch.setattr(scheduler.runner, "run_worker", fake_run_worker)

        result = run_engage(store, cfg, finding)
        assert result.get("outcome") == "engaged", result
        assert result.get("pushed") is True

        upstream_path = Path(store.get_repo(finding["repo_id"])["url"])
        rc = subprocess.run(
            ["git", "log", "--oneline", "feature"],
            cwd=upstream_path,
            capture_output=True,
            text=True,
            check=True,
        )
        assert "squashed: clean commit for merge" in rc.stdout
        assert "first published commit" not in rc.stdout
        assert any("Rebased and squashed as requested." in c for c in fake_forge.comments)


class TestEngageNeverTargetsDefaultBranch:
    def test_refuses_when_head_ref_is_default_branch(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """However head_ref got set to the default branch (shouldn't happen
        structurally, but cheap to refuse outright), run_engage must bail
        before touching git or spending any worker tokens."""
        finding, fake_forge = _setup(store, tmp_path, head_ref="main")
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        called = False

        def fake_run_worker(*_a: object, **_kw: object) -> RunResult:
            nonlocal called
            called = True
            return _fake_result()

        monkeypatch.setattr(scheduler.runner, "run_worker", fake_run_worker)

        result = run_engage(store, cfg, finding)
        assert "error" in result
        assert not called, "worker must never run when head_ref is the default branch"
