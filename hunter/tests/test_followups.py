"""Deferred-work follow-ups: apply_improvement.md workers can defer a bigger
migration (e.g. "dep upgraded but the DSL migration it enables is left for
later") -- that used to live only as prose in a PR that closes out and stops
being watched the moment it merges. run_fix now ingests an optional
FOLLOW-UPS.json the worker writes alongside PR-DESCRIPTION.md into the finding
queue (as type="modernization") right after a successful ship, so deferred
work becomes a normal, triage-able finding instead of a note no one re-reads.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import run_fix
from hunter.store import Store
from hunter.types import Config, RunResult


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)


def _run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _make_repo(tmp_path: Path) -> tuple[Path, Path]:
    seed = tmp_path / "seed"
    seed.mkdir()
    _run("git", "init", "-b", "main", cwd=seed)
    _run("git", "config", "user.email", "t@t.com", cwd=seed)
    _run("git", "config", "user.name", "t", cwd=seed)
    (seed / "build.gradle.kts").write_text("android {}\n")
    _run("git", "add", ".", cwd=seed)
    _run("git", "commit", "-m", "init", cwd=seed)

    upstream_path = tmp_path / "upstream.git"
    _run("git", "clone", "--bare", str(seed), str(upstream_path), cwd=tmp_path)
    repo_path = tmp_path / "repo"
    _run("git", "clone", str(upstream_path), str(repo_path), cwd=tmp_path)
    _run("git", "config", "user.email", "t@t.com", cwd=repo_path)
    _run("git", "config", "user.name", "t", cwd=repo_path)
    return upstream_path, repo_path


def _make_finding(**overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "type": "dep_update",
        "fingerprint": "repo:npm:agp:8.0.0->8.5.0",
        "file": "build.gradle.kts",
        "ecosystem": "gradle",
        "package": "com.android.tools.build:gradle",
        "current_version": "8.0.0",
        "latest_version": "8.5.0",
        "update_type": "minor",
        "severity": "medium",
        "confidence": 0.9,
        "summary": "Bump AGP to 8.5.0",
        "detail": "Newer AGP with bugfixes",
    }
    base.update(overrides)
    return base


def _setup(store: Store, tmp_path: Path) -> dict[str, Any]:
    upstream_path, repo_path = _make_repo(tmp_path)
    repo_id = store.add_repo("repo", str(upstream_path), str(repo_path), default_branch="main")
    fid, _ = store.upsert_finding(repo_id, _make_finding(), finding_type="dep_update")
    store.set_status(fid, "queued")
    finding = store.get_finding(fid)
    assert finding is not None
    finding["budget_override"] = "exempt"  # bypass "no window data" denial
    return finding


class _FakeForge:
    def ssh_url(self, url: str) -> str:
        return url

    def owner_repo(self, url: str) -> str:
        return "owner/repo"

    def create_pr(self, *a: object, **kw: object) -> tuple[int, str]:
        return 0, "https://example.com/pull/1"


def _worker_with_followups(followups: list[dict[str, Any]] | None) -> Any:
    def worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
        (worktree / "build.gradle.kts").write_text("android { newDsl = false }\n")
        _run("git", "add", "-A", cwd=worktree)
        _run("git", "commit", "-m", "deps: bump AGP to 8.5.0", cwd=worktree)
        (worktree / "PR-DESCRIPTION.md").write_text(
            "Bumped AGP to 8.5.0. New DSL migration deferred via android.newDsl=false.\n"
        )
        if followups is not None:
            (worktree / "FOLLOW-UPS.json").write_text(json.dumps(followups))
        return RunResult(
            exit_code=0,
            killed_reason=None,
            tokens_new=1000,
            calls=1,
            session_file=None,
            duration_s=5.0,
            stdout_tail="done",
        )

    return worker


class TestFollowUpIngestion:
    def test_followups_ingested_as_modernization_finding_on_ship(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding = _setup(store, tmp_path)
        followups = [
            {
                "fingerprint": "repo:android-dsl:new-dsl-migration",
                "file": "build.gradle.kts",
                "modernization_class": "deferred-followup",
                "current_approach": "Legacy Android DSL; android.newDsl=false",
                "proposed_approach": "Migrate incrementally to AndroidComponentsExtension",
                "severity": "low",
                "confidence": 0.7,
                "summary": "New DSL migration deferred during AGP bump",
                "detail": "Deferred to keep the dep bump safe; needs a dedicated pass.",
                "introduced_by": f"deferred while applying dep_update {finding['fingerprint']}",
            }
        ]
        monkeypatch.setattr(scheduler.runner, "run_worker", _worker_with_followups(followups))
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: _FakeForge())

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "pr_open", result
        all_findings = store.list_all_findings()
        modernization = [f for f in all_findings if f["type"] == "modernization"]
        assert len(modernization) == 1, all_findings
        mf = modernization[0]
        assert mf["fingerprint"] == "repo:android-dsl:new-dsl-migration"
        assert mf["status"] == "new"
        assert mf["modernization_class"] == "deferred-followup"
        assert mf["introduced_by"] == f"deferred while applying dep_update {finding['fingerprint']}"

    def test_no_followups_file_ships_normally_with_no_extra_findings(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The mechanism is opt-in: no FOLLOW-UPS.json -> no behavior change."""
        finding = _setup(store, tmp_path)
        monkeypatch.setattr(scheduler.runner, "run_worker", _worker_with_followups(None))
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: _FakeForge())

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "pr_open", result
        all_findings = store.list_all_findings()
        assert all(f["type"] != "modernization" for f in all_findings)

    def test_invalid_followup_entry_is_skipped_not_fatal(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A malformed entry (missing fingerprint) must not block the ship
        that already succeeded -- ingestion happens after set_status(pr_open)."""
        finding = _setup(store, tmp_path)
        followups = [{"summary": "no fingerprint here"}]
        monkeypatch.setattr(scheduler.runner, "run_worker", _worker_with_followups(followups))
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: _FakeForge())

        result = run_fix(store, cfg, finding)

        assert result.get("outcome") == "pr_open", result
        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "pr_open"
        all_findings = store.list_all_findings()
        assert all(f["type"] != "modernization" for f in all_findings)
