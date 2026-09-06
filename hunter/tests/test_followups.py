"""Deferred-work follow-ups: a worker (apply_improvement.md's fix path, or
engage.md's withdraw path) can discover real follow-up work -- "dep upgraded
but the DSL migration it enables is left for later", or "the PR I'm closing
is superseded, but that also means the original target is now MORE
achievable, not less" -- that used to live only as prose in a PR/comment
that stops being watched the moment it merges or closes. Both run_fix (on
ship) and run_engage (on withdraw) now ingest an optional FOLLOW-UPS.json
the worker writes into the finding queue, so deferred/reopened work becomes
a normal, triage-able finding instead of a note no one re-reads.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter import scheduler
from hunter.scheduler import run_engage, run_fix
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
                "type": "modernization",
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


def _make_repo_with_published_branch(tmp_path: Path) -> tuple[Path, Path]:
    seed = tmp_path / "seed"
    seed.mkdir()
    _run("git", "init", "-b", "main", cwd=seed)
    _run("git", "config", "user.email", "t@t.com", cwd=seed)
    _run("git", "config", "user.name", "t", cwd=seed)
    (seed / "build.gradle.kts").write_text("android {}\n")
    _run("git", "add", ".", cwd=seed)
    _run("git", "commit", "-m", "init", cwd=seed)
    _run("git", "checkout", "-b", "feature", cwd=seed)
    (seed / "build.gradle.kts").write_text("hilt = '2.55'\n")
    _run("git", "add", ".", cwd=seed)
    _run("git", "commit", "-m", "deps: bump hilt to 2.55", cwd=seed)
    _run("git", "checkout", "main", cwd=seed)

    upstream_path = tmp_path / "upstream.git"
    _run("git", "clone", "--bare", str(seed), str(upstream_path), cwd=tmp_path)
    repo_path = tmp_path / "repo"
    _run("git", "clone", str(upstream_path), str(repo_path), cwd=tmp_path)
    _run("git", "config", "user.email", "t@t.com", cwd=repo_path)
    _run("git", "config", "user.name", "t", cwd=repo_path)
    return upstream_path, repo_path


class _EngageFakeForge:
    def __init__(self) -> None:
        self.closed: list[tuple[int, str]] = []

    def owner_repo(self, url: str) -> str:
        return "owner/repo"

    def ssh_url(self, https_url: str) -> str:
        return https_url

    def view_pr_engage(self, slug: str, number: int, timeout: int = 30) -> Any:
        return 0, {"title": "deps: upgrade hilt to 2.55", "body": "b"}, ""

    def close_pr(self, slug: str, number: int, comment: str, timeout: int = 60) -> tuple[int, str]:
        self.closed.append((number, comment))
        return 0, ""


def _setup_engage(store: Store, tmp_path: Path, **finding_overrides: Any) -> dict[str, Any]:
    upstream_path, repo_path = _make_repo_with_published_branch(tmp_path)
    repo_id = store.add_repo("repo", str(upstream_path), str(repo_path), default_branch="main")
    fid, _ = store.upsert_finding(
        repo_id,
        _make_finding(
            fingerprint="repo:gradle:com.google.dagger:hilt-android:2.48->2.60.1",
            **finding_overrides,
        ),
        finding_type="dep_update",
    )
    store.set_status(fid, "pr_open")
    store.upsert_pr_state(
        fid,
        pr_number=1,
        head_ref="feature",
        needs_attention="conflict",
        synced_at=0,
    )
    finding = store.get_finding(fid)
    assert finding is not None
    finding["budget_override"] = "exempt"
    return finding


def _engage_worker_with_followups(followups: list[dict[str, Any]] | None) -> Any:
    def worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
        (worktree / "WITHDRAW.md").write_text(
            "This PR is superseded. origin/main already upgraded Hilt to 2.56.2"
            " as part of a coordinated Kotlin/KSP bump; our target of 2.55 is"
            " strictly behind. But 2.60.1 -- the original goal -- is now"
            " achievable since the Kotlin/KSP blocker is resolved.\n"
        )
        if followups is not None:
            (worktree / "FOLLOW-UPS.json").write_text(json.dumps(followups))
        return RunResult(
            exit_code=0,
            killed_reason=None,
            tokens_new=500,
            calls=1,
            session_file=None,
            duration_s=2.0,
            stdout_tail="done",
        )

    return worker


class TestEngageWithdrawFollowUpIngestion:
    def test_superseded_withdraw_files_dep_update_followup(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The exact glosdalen Hilt/Kotlin scenario: a PR capped at 2.55 gets
        superseded by a coordinated bump to 2.56.2 -- but that same bump
        resolves the constraint that capped the original PR, so 2.60.1 (the
        finding's actual original target) is achievable again. Withdrawing
        must not silently drop that -- it must re-propose 2.60.1 as a fresh
        finding."""
        finding = _setup_engage(store, tmp_path)
        followups = [
            {
                "type": "dep_update",
                "fingerprint": "repo:gradle:com.google.dagger:hilt-android:2.56.2->2.60.1",
                "ecosystem": "gradle",
                "package": "com.google.dagger:hilt-android",
                "current_version": "2.56.2",
                "latest_version": "2.60.1",
                "update_type": "minor",
                "severity": "medium",
                "confidence": 0.8,
                "summary": "Hilt 2.60.1 now achievable now that Kotlin/KSP moved",
                "detail": "The original PR capped at 2.55 due to a KSP/Kotlin"
                " mismatch; that mismatch is now resolved by the coordinated"
                " bump on main, so the original 2.60.1 target is open again.",
                "introduced_by": f"reopened while withdrawing {finding['fingerprint']}",
            }
        ]
        monkeypatch.setattr(
            scheduler.runner, "run_worker", _engage_worker_with_followups(followups)
        )
        fake_forge = _EngageFakeForge()
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        result = run_engage(store, cfg, finding)

        assert result.get("outcome") == "withdrawn", result
        assert fake_forge.closed, "close_pr must have been called"
        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "rejected"

        all_findings = store.list_all_findings()
        dep_updates = [
            f for f in all_findings if f["type"] == "dep_update" and f["id"] != finding["id"]
        ]
        assert len(dep_updates) == 1, all_findings
        assert dep_updates[0]["fingerprint"] == (
            "repo:gradle:com.google.dagger:hilt-android:2.56.2->2.60.1"
        )
        assert dep_updates[0]["status"] == "new"

    def test_withdraw_without_followups_is_unaffected(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A genuinely fully-superseded withdrawal (nothing further to
        propose) must behave exactly as before -- opt-in, no new findings."""
        finding = _setup_engage(store, tmp_path)
        monkeypatch.setattr(scheduler.runner, "run_worker", _engage_worker_with_followups(None))
        fake_forge = _EngageFakeForge()
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        result = run_engage(store, cfg, finding)

        assert result.get("outcome") == "withdrawn", result
        after = store.get_finding(finding["id"])
        assert after is not None
        assert after["status"] == "rejected"
        all_findings = store.list_all_findings()
        assert len(all_findings) == 1  # only the original finding, nothing new
