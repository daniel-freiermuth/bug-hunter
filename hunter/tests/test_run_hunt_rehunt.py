"""Tests for run_hunt's last_full_hunt_at seeding.

Without seeding it on a repo's first completed hunt, last_full_hunt_at
stays NULL forever: rehunt_due (the gate for cfg.hunt_rehunt_days's
periodic full re-hunt) requires it non-null, but the ONLY other writer
sits behind rehunt_due itself -- a chicken-and-egg deadlock that would
silently disable the periodic full re-hunt feature for every repo.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest

from hunter import scheduler
from hunter.scheduler import run_hunt
from hunter.store import Store
from hunter.types import BudgetDecision, Config, RunResult


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)


def _run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _make_repo(tmp_path: Path) -> tuple[Path, Path]:
    """Bare upstream + local clone with 'origin' configured, matching
    what run_hunt's `git fetch origin` needs."""
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


def _empty_worker(_cfg: Config, _rpath: Path, prompt: str, *_a: object, **_kw: object) -> RunResult:
    """Write the empty findings.json the real hunt playbook's output
    contract requires as the worker's first action, so run_hunt sees a
    genuine clean-scan completion (state == 'done', output present)."""
    m = re.search(r"Create (\S+) containing", prompt)
    if m:
        out_path = Path(m.group(1))
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text("[]")
    return RunResult(
        exit_code=0,
        killed_reason=None,
        tokens_new=100,
        calls=1,
        session_file=None,
        duration_s=1.0,
        stdout_tail="no findings",
    )


def _allow_everything(*_a: object, **_kw: object) -> BudgetDecision:
    return BudgetDecision(True, "ok", 200_000)


class TestRehuntSeeding:
    def test_first_hunt_seeds_last_full_hunt_at(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        upstream_path, repo_path = _make_repo(tmp_path)
        rid = store.add_repo("r", str(upstream_path), str(repo_path), default_branch="main")
        repo = store.get_repo(rid)
        assert repo is not None
        assert repo["last_full_hunt_at"] is None

        monkeypatch.setattr(scheduler.runner, "run_worker", _empty_worker)
        monkeypatch.setattr(scheduler.budget, "decide", _allow_everything)

        result = run_hunt(store, cfg, repo)
        assert "error" not in result, result
        assert result["state"] == "done"

        after = store.get_repo(rid)
        assert after is not None
        assert after["last_full_hunt_at"] is not None, (
            "last_full_hunt_at must be seeded on the first completed hunt, or"
            " rehunt_due can never become true for this repo"
        )

    def test_ordinary_followup_hunt_does_not_re_seed(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Once seeded, a later ordinary (non-full-rehunt) hunt must leave
        the clock alone -- only a genuine full-rehunt completion should
        ever advance it again."""
        upstream_path, repo_path = _make_repo(tmp_path)
        rid = store.add_repo("r", str(upstream_path), str(repo_path), default_branch="main")
        monkeypatch.setattr(scheduler.runner, "run_worker", _empty_worker)
        monkeypatch.setattr(scheduler.budget, "decide", _allow_everything)

        repo = store.get_repo(rid)
        assert repo is not None
        run_hunt(store, cfg, repo)
        seeded_at = store.get_repo(rid)["last_full_hunt_at"]  # type: ignore[index]
        assert seeded_at is not None

        (repo_path / "f.py").write_text("pass\nmore\n")
        _run("git", "add", "-A", cwd=repo_path)
        _run("git", "commit", "-m", "more", cwd=repo_path)
        _run("git", "push", "origin", "main", cwd=repo_path)
        repo2 = store.get_repo(rid)
        assert repo2 is not None
        run_hunt(store, cfg, repo2)

        assert store.get_repo(rid)["last_full_hunt_at"] == seeded_at  # type: ignore[index]
