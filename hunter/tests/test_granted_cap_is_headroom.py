"""What the scheduler grants a job is the backend's headroom, verbatim.

The cap used to be min(configured per-kind cap, backend headroom), with
the configured number standing in whenever the backend named none.  That
number was fixed while the ramp's headroom is live, and it sat below the
measured typical cost of the work it governed, so it killed jobs the
budget had already funded.  The headroom is now the whole answer --
including when there is no figure, which means no token bound.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest

from hunter.backend import Granted, Outlook
from hunter.scheduler import run_hunt
from hunter.store import Store
from hunter.types import Config, RunResult

# Above the 200_000 per-kind cap this daemon shipped with, so a surviving
# min() shows up as a smaller number rather than as an equal one.
_BIG_HEADROOM = 900_000


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


class _RecordingBackend:
    """Grants exactly `headroom` and remembers what run() was handed."""

    def __init__(self, headroom: int | None) -> None:
        self._headroom = headroom
        self.ran_with: list[int | None] = []

    def decide(self, *, anticipated_tokens: int = 0) -> Outlook:
        granted = Granted(cap_tokens=self._headroom)
        return Outlook(normal=granted, prioritized=granted)

    def run(
        self,
        cwd: Path,
        prompt: str,
        *,
        cap_tokens: int | None,
        max_wall_s: float,
        job_class: object,
    ) -> RunResult:
        self.ran_with.append(cap_tokens)
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

    def keep_fresh(self) -> bool:
        return False

    def status(self) -> str:
        return ""


def _hunt(store: Store, cfg: Config, tmp_path: Path, backend: _RecordingBackend) -> int:
    upstream_path, repo_path = _make_repo(tmp_path)
    rid = store.add_repo("r", str(upstream_path), str(repo_path), default_branch="main")
    repo = store.get_repo(rid)
    assert repo is not None
    result = run_hunt(store, cfg, repo, backend)
    assert "error" not in result, result
    return int(result["job"])  # type: ignore[arg-type]


def _job_cap(store: Store, job_id: int) -> int | None:
    row = store.db.execute("SELECT cap_tokens FROM jobs WHERE id = ?", (job_id,)).fetchone()
    assert row is not None
    return None if row["cap_tokens"] is None else int(row["cap_tokens"])


def test_headroom_above_the_old_configured_cap_is_granted_whole(
    store: Store, cfg: Config, tmp_path: Path
) -> None:
    backend = _RecordingBackend(_BIG_HEADROOM)

    job = _hunt(store, cfg, tmp_path, backend)

    assert backend.ran_with == [_BIG_HEADROOM]
    assert _job_cap(store, job) == _BIG_HEADROOM


def test_a_grant_with_no_headroom_figure_stores_no_cap(
    store: Store, cfg: Config, tmp_path: Path
) -> None:
    """None is "no token bound", not "fall back to a configured number":
    the worker gets None and the row keeps NULL."""
    backend = _RecordingBackend(None)

    job = _hunt(store, cfg, tmp_path, backend)

    assert backend.ran_with == [None]
    assert _job_cap(store, job) is None
