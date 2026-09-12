"""Proof, not just a repair: a finding must never be observable at status
'fixing' outside the dynamic extent of a single run_fix call, because the
normal work queue (run_cycle's priority scan) never looks for 'fixing' --
anything left there is not "retried later", it silently disappears.

Store.in_progress() (a try/finally context manager) is the structural
mechanism: it guarantees that however the guarded block exits -- normal
return, early return, or an exception raised ANYWHERE inside it -- the
finding is never left at 'fixing'. This file proves that by injecting a
raised exception at several distinct points along run_fix's own call
chain (the worker call itself, immediately after it returns, and deep in
the git-push/PR-create post-processing -- the exact point that orphaned
a real production finding, see hunter #2030-shaped incidents) and
asserting the invariant holds regardless of where the fault originates.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import run_fix
from hunter.store import Store
from hunter.types import Config, RunResult
from hunter.backend import Granted, Outlook


class _FakeBackend:
    """Minimal Backend for tests: always grants, delegates run() to a worker fn."""

    def __init__(self, worker_fn: object) -> None:
        self._fn = worker_fn

    def decide(self, *, anticipated_tokens: int = 0) -> Outlook:
        return Outlook(normal=Granted(cap_tokens=200_000), prioritized=Granted(cap_tokens=200_000))

    def run(self, cwd: Path, prompt: str, *, cap_tokens: int, max_wall_s: float, job_class: object) -> RunResult:
        return self._fn(None, cwd, prompt, cap_tokens, max_wall_s)  # type: ignore[misc]

    def keep_fresh(self) -> bool:
        return False

    def status(self) -> str:
        return ""


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
    """Bare upstream + local clone with a 'main' branch, matching what
    run_fix needs to create its worktree/branch from origin/main."""
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
    repo_id = store.add_repo(
        "repo", str(upstream_path), str(repo_path), default_branch="main"
    )
    fid, _ = store.upsert_finding(repo_id, _make_finding())
    store.set_status(fid, "queued")
    finding = store.get_finding(fid)
    assert finding is not None
    finding["budget_override"] = "exempt"  # bypass "no window data" denial
    return finding


def _worker_that_ships_a_commit(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
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


class TestFixingStatusNeverStranded:
    def test_happy_path_ships_pr_open(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Sanity: the normal success path still reaches pr_open (not
        left at 'fixing', not incorrectly requeued)."""
        finding = _setup(store, tmp_path)
        backend = _FakeBackend(_worker_that_ships_a_commit)

        class FakeForge:
            def ssh_url(self, url: str) -> str:
                return url

            def owner_repo(self, url: str) -> str:
                return "owner/repo"

            def create_pr(self, *a: object, **kw: object) -> tuple[int, str]:
                return 0, "https://example.com/pull/1"

        monkeypatch.setattr(scheduler, "forge_for", lambda repo: FakeForge())

        result = run_fix(store, cfg, finding, backend)

        assert result.get("outcome") == "pr_open", result
        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "pr_open"

    def test_exception_inside_worker_call_does_not_strand_fixing(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """Fault point 1: the worker call itself raises (not a killed
        RunResult -- an actual Python exception, e.g. a transport error)."""
        finding = _setup(store, tmp_path)

        def raising_worker(*_a: object, **_kw: object) -> RunResult:
            raise RuntimeError("simulated transport failure")

        with pytest.raises(RuntimeError, match="simulated transport failure"):
            run_fix(store, cfg, finding, _FakeBackend(raising_worker))

        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "queued", (
            f"finding stuck at {after['status']!r} after a raised exception mid-fix"
        )

    def test_exception_right_after_worker_returns_does_not_strand_fixing(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Fault point 2: _record_job itself raises -- covers the window
        between the worker returning and the job's own state being
        written at all."""
        finding = _setup(store, tmp_path)
        backend = _FakeBackend(_worker_that_ships_a_commit)

        def raising_record_job(*_a: object, **_kw: object) -> str:
            raise RuntimeError("simulated DB write failure")

        monkeypatch.setattr(scheduler, "_record_job", raising_record_job)

        with pytest.raises(RuntimeError, match="simulated DB write failure"):
            run_fix(store, cfg, finding, backend)

        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "queued"

    def test_exception_deep_in_pr_creation_does_not_strand_fixing(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Fault point 3: the exact shape of the real production gap --
        _record_job already succeeded (job row would be terminal), but
        the git-push/PR-create code that follows raises before reaching
        its own set_status call."""
        finding = _setup(store, tmp_path)
        backend = _FakeBackend(_worker_that_ships_a_commit)

        class RaisingForge:
            def ssh_url(self, url: str) -> str:
                return url

            def owner_repo(self, url: str) -> str:
                return "owner/repo"

            def create_pr(self, *a: object, **kw: object) -> tuple[int, str]:
                raise RuntimeError("simulated gh api outage")

        monkeypatch.setattr(scheduler, "forge_for", lambda repo: RaisingForge())

        with pytest.raises(RuntimeError, match="simulated gh api outage"):
            run_fix(store, cfg, finding, backend)

        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "queued", (
            f"finding stuck at {after['status']!r} -- this is the exact bug class "
            "that stranded a real production finding for a month"
        )
