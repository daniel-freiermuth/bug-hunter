"""Which job turned a finding up, end to end.

Nothing recorded this: the ingest path had the job id in hand and passed
it to every event it logged except the one that matters -- the "new
<type>" event marking the finding's creation got NULL -- so the job that
found a bug and the bug itself were never connected in the data at all.
Answering "what did this hunt turn up?" meant reading the "+2 new"
counter out of a log message and searching by timestamp.

The attribution is written once, on the insert. A hunt over unchanged
code re-reports every finding it reported last time, so crediting the
rediscoverer would move ownership forward on every cycle and leave no
job able to say what it actually contributed.
"""

from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path

import pytest

from hunter.backend import Granted, Outlook
from hunter.scheduler import run_hunt
from hunter.store import Store
from hunter.types import Config, Row, RunResult

_FINDINGS = [
    {
        "fingerprint": "repo:f.py:one:logic",
        "file": "f.py",
        "symbol": "one",
        "bug_class": "logic",
        "severity": "high",
        "confidence": 0.9,
        "summary": "off-by-one in one()",
    },
    {
        "fingerprint": "repo:f.py:two:error-path",
        "file": "f.py",
        "symbol": "two",
        "bug_class": "error-path",
        "severity": "medium",
        "confidence": 0.8,
        "summary": "two() swallows the error",
    },
]


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)


def _run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _make_repo(tmp_path: Path) -> Path:
    """Bare upstream run_hunt can clone and fetch from."""
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
    return upstream_path


def _finding_worker(
    _cfg: Config, _rpath: Path, prompt: str, *_a: object, **_kw: object
) -> RunResult:
    """Report the same two findings every run, as a hunt over unchanged
    code does, writing them where the prompt's output contract says."""
    m = re.search(r"Create (\S+) containing", prompt)
    assert m, "the hunt prompt must name the findings file"
    out_path = Path(m.group(1))
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(_FINDINGS))
    return RunResult(
        exit_code=0,
        killed_reason=None,
        tokens_new=100,
        calls=1,
        session_file=None,
        duration_s=1.0,
        stdout_tail="2 findings",
    )


class _FakeBackend:
    """Minimal Backend for tests: always grants, delegates run() to a worker fn."""

    def __init__(self, worker_fn: object) -> None:
        self._fn = worker_fn

    def decide(self, *, anticipated_tokens: int = 0) -> Outlook:
        return Outlook(normal=Granted(cap_tokens=200_000), prioritized=Granted(cap_tokens=200_000))

    def run(
        self,
        cwd: Path,
        prompt: str,
        *,
        cap_tokens: int,
        max_wall_s: float,
        job_class: object,
        resume_from: Path | None = None,
    ) -> RunResult:
        return self._fn(None, cwd, prompt, cap_tokens, max_wall_s)  # type: ignore[misc]

    def keep_fresh(self) -> bool:
        return False

    def status(self) -> str:
        return ""


def _produced(store: Store) -> dict[int, list[int]]:
    """/api/jobs' view: job id -> the findings it brought into existence."""
    return {j["id"]: j["produced_finding_ids"] for j in store.list_jobs()}


def test_findings_belong_to_the_hunt_that_first_found_them(
    store: Store, cfg: Config, tmp_path: Path
) -> None:
    upstream = _make_repo(tmp_path)
    rid = store.add_repo("r", str(upstream), str(tmp_path / "repos"), default_branch="main")
    repo: Row | None = store.get_repo(rid)
    assert repo is not None
    backend = _FakeBackend(_finding_worker)

    first = run_hunt(store, cfg, repo, backend)
    assert first.get("ingest") == {"inserted": 2, "duplicates": 0, "invalid": 0}, first
    fids = sorted(f["id"] for f in store.list_findings())
    assert len(fids) == 2
    assert _produced(store)[first["job"]] == fids

    repo = store.get_repo(rid)
    assert repo is not None
    second = run_hunt(store, cfg, repo, backend, force=True)
    assert second.get("ingest") == {"inserted": 0, "duplicates": 2, "invalid": 0}, second
    produced = _produced(store)
    assert produced[second["job"]] == []
    assert produced[first["job"]] == fids

    # A job handed a finding consumes one rather than producing any, and
    # says so with an empty list instead of an absent key.
    fix = store.create_job("fix", rid, finding_id=fids[0])
    assert _produced(store)[fix] == []
