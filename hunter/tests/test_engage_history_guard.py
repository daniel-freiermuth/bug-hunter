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
from hunter.scheduler import run_engage, sync_prs
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


class _FakeForge:
    """Minimal Forge stand-in: no network, records posted comments."""

    def __init__(self) -> None:
        self.comments: list[str] = []
        self.checks_failing = False  # only consulted by view_pr_sync/parse_pr_url callers
        self.head_sha = "sha1"  # override to simulate a human push between syncs

    def owner_repo(self, url: str) -> str:
        return "owner/repo"

    def ssh_url(self, https_url: str) -> str:
        return https_url

    def view_pr_engage(self, slug: str, number: int, timeout: int = 30):
        return 0, {"title": "t", "body": "b"}, ""

    def comment_pr(self, slug: str, number: int, body_file: Path, timeout: int = 60):
        self.comments.append(Path(body_file).read_text())
        return 0, ""

    def parse_pr_url(self, url: str):
        return "owner/repo", 1

    def view_pr_sync(self, slug: str, number: int, timeout: int = 30):
        rollup = [{"name": "CI", "conclusion": "FAILURE"}] if self.checks_failing else []
        return (
            0,
            {
                "state": "OPEN",
                "comments": [],
                "reviews": [],
                "reviewDecision": None,
                "mergeable": "MERGEABLE",
                "statusCheckRollup": rollup,
                "updatedAt": "2026-01-01T00:00:00Z",
                "headRefName": "feature",
                "headRefOid": self.head_sha,
            },
            "",
        )


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

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
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

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
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

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
        assert "error" in result
        assert not called, "worker must never run when head_ref is the default branch"


class TestEngageAddressedFingerprint:
    """The other half of the fairness/loop fix (see
    sync_prs's addressed_fingerprint comparison): a reply that changes
    nothing must not let the identical still-outstanding static reason
    (which checks fail, review state, conflict) re-trigger next cycle.
    State-based, not time-based: no re-poking a worker already explained
    itself on, no matter how long the daemon then runs unattended, until
    the actual situation changes. Reproduces the production incident
    directly -- recentIP PR #6 replied to an unresolved 'checks_failing'
    five times in ~7 minutes because nothing ever suppressed the
    immediate re-flag."""

    def test_addressed_fingerprint_is_set_when_replying_without_pushing(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        finding, fake_forge = _setup(store, tmp_path)
        store.upsert_pr_state(finding["id"], attention_fingerprint="checks:CI")
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        def fake_run_worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
            # No commits -- just a decline/explanation, matching the
            # observed "pre-existing on main, not this PR's fault" reply.
            (worktree / "PR-REPLY.md").write_text("This is pre-existing, not caused by this PR.\n")
            return _fake_result()

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
        assert result.get("outcome") == "engaged", result
        assert result.get("pushed") is False

        ps = store.get_pr_state(finding["id"])
        assert ps is not None
        # needs_attention is deliberately left as-is -- sync_prs (which
        # always runs before pick_next) is the sole authority on it;
        # run_engage overwriting it here previously destroyed the
        # continuity attention_since/the fingerprint mechanism depend on.
        assert ps["addressed_fingerprint"] == "checks:CI"

    def test_addressed_fingerprint_is_cleared_when_commits_are_pushed(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A genuine attempt (a push) clears any stale marker from an
        earlier decline -- the underlying problem may actually be
        resolved now, worth a genuinely fresh look next sync, not
        suppressed by memory of what was declined before."""
        finding, fake_forge = _setup(store, tmp_path)
        store.upsert_pr_state(
            finding["id"],
            attention_fingerprint="checks:CI",
            addressed_fingerprint="checks:CI",  # stale, from an earlier decline
        )
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        def fake_run_worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
            (worktree / "f.py").write_text("pass\npublished\nfixed\n")
            _run("git", "add", "-A", cwd=worktree)
            _run("git", "commit", "-m", "fix the actual problem", cwd=worktree)
            (worktree / "PR-REPLY.md").write_text("Fixed.\n")
            return _fake_result()

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
        assert result.get("outcome") == "engaged", result
        assert result.get("pushed") is True

        ps = store.get_pr_state(finding["id"])
        assert ps is not None
        assert ps["addressed_fingerprint"] is None

    def test_addressed_fingerprint_is_set_on_pure_no_op(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Neither pushed nor replied (nothing needed doing) is exactly
        as unproductive as a decline-only reply and must record the
        fingerprint too."""
        finding, fake_forge = _setup(store, tmp_path)
        store.upsert_pr_state(finding["id"], attention_fingerprint="mergeable:CONFLICTING")
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)
        backend = _FakeBackend(lambda *_a, **_kw: _fake_result())
        result = run_engage(store, cfg, finding, backend)
        assert result.get("outcome") == "engaged", result
        assert result.get("pushed") is False
        assert result.get("replied") is False

        ps = store.get_pr_state(finding["id"])
        assert ps is not None
        assert ps["addressed_fingerprint"] == "mergeable:CONFLICTING"

    def test_suppression_survives_the_next_sync_prs_pass(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The exact bug caught live seconds after an earlier version of
        this fix first shipped: that version cleared needs_attention to
        None in run_engage, which made the VERY NEXT sync_prs pass see a
        false None -> reason transition (since the still-failing check
        computes the same reason again) and treat it as genuinely new,
        wiping the suppression this same call had just set. This is the
        actual production sequence -- sync_prs, then run_engage, then
        sync_prs again -- not just each function tested in isolation.
        Also proves the suppression has no time component at all: this
        test asserts no expiry because the mechanism has none to expire
        -- unlike a time-based backoff, there is nothing here for a
        months-long unattended daemon run to eventually re-trigger."""
        finding, fake_forge = _setup(store, tmp_path)
        fake_forge.checks_failing = True
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        sync_prs(store, cfg)
        before = store.get_pr_state(finding["id"])
        assert before is not None
        assert before["needs_attention"] == "checks_failing"
        assert before["attention_fingerprint"] == "checks:CI"

        def fake_run_worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
            (worktree / "PR-REPLY.md").write_text("This is a pre-existing failure, not mine.\n")
            return _fake_result()

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
        assert result.get("outcome") == "engaged", result
        assert result.get("pushed") is False

        # The check is STILL failing (nothing was pushed) -- exactly the
        # production scenario. This is the pass that used to wipe the
        # suppression.
        sync_prs(store, cfg)

        after = store.get_pr_state(finding["id"])
        assert after is not None
        assert after["addressed_fingerprint"] == "checks:CI"
        assert after["needs_attention"] is None  # suppressed, not re-flagged
        assert store.list_attention() == []  # correctly suppressed, not re-picked

    def test_human_push_with_same_failing_check_is_not_suppressed(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The gap this fix closes: fingerprint-only suppression can't
        distinguish 'nothing changed since the decline' from 'a human
        pushed real new code that happens to leave the same check red'.
        The full production sequence -- sync_prs, no-push decline via
        run_engage, a genuine human push, sync_prs again -- must re-flag
        once the head sha moves, even though the static fingerprint
        string is identical both times."""
        finding, fake_forge = _setup(store, tmp_path)
        fake_forge.checks_failing = True
        fake_forge.head_sha = "sha1"
        monkeypatch.setattr(scheduler, "forge_for", lambda repo: fake_forge)

        sync_prs(store, cfg)
        before = store.get_pr_state(finding["id"])
        assert before is not None
        assert before["head_sha"] == "sha1"

        def fake_run_worker(_cfg: Config, worktree: Path, *_a: object, **_kw: object) -> RunResult:
            (worktree / "PR-REPLY.md").write_text("This is a pre-existing failure, not mine.\n")
            return _fake_result()

        backend = _FakeBackend(fake_run_worker)
        result = run_engage(store, cfg, finding, backend)
        assert result.get("pushed") is False

        declined = store.get_pr_state(finding["id"])
        assert declined is not None
        assert declined["addressed_fingerprint"] == "checks:CI"
        assert declined["addressed_head_sha"] == "sha1"

        # A human pushes new code; the (different) new code happens to
        # still fail the same-named check.
        fake_forge.head_sha = "sha2"
        sync_prs(store, cfg)

        after = store.get_pr_state(finding["id"])
        assert after is not None
        assert after["attention_fingerprint"] == "checks:CI"  # same-looking reason
        assert after["needs_attention"] == "checks_failing", (
            "a human push must re-flag even when the static reason string is unchanged"
        )
        assert after["addressed_fingerprint"] is None
        assert after["addressed_head_sha"] is None
        assert store.list_attention() != []
