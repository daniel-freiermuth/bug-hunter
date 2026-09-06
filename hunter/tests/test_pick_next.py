"""Tests for hunter.scheduler.pick_next -- the single, pure, side-effect-
free selection function shared by run_cycle (execution) and the Status
page's "what's next" preview (display). Testing it directly, rather than
only through run_cycle's side effects, is what lets the preview be
proven to match reality instead of merely hoped to."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from hunter.scheduler import pick_next
from hunter.store import Store
from hunter.types import Config, now_ms


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


class TestPickNextEmpty:
    def test_nothing_at_all_returns_none(self, store: Store, cfg: Config) -> None:
        assert pick_next(store, cfg) is None

    def test_disabled_repo_is_ignored(self, store: Store, cfg: Config) -> None:
        rid = store.add_repo("r", "https://r", "/nonexistent")
        store.update_repo(rid, enabled=False)
        assert pick_next(store, cfg) is None


class TestPickNextPriority:
    def test_attention_beats_rechecking_and_queued_and_repos(
        self, store: Store, cfg: Config
    ) -> None:
        rid = store.add_repo("r", "https://r", "/nonexistent")
        fid_r, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-recheck"))
        store.set_status(fid_r, "rechecking")
        fid_q, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-queued"))
        store.set_status(fid_q, "queued")
        fid_a, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-attn"))
        store.set_status(fid_a, "pr_open")
        store.upsert_pr_state(fid_a, pr_number=1, needs_attention="new_comments", synced_at=1)

        kind, target = pick_next(store, cfg)
        assert kind == "engage"
        assert target["id"] == fid_a

    def test_rechecking_beats_queued_and_repos(self, store: Store, cfg: Config) -> None:
        rid = store.add_repo("r", "https://r", "/nonexistent")
        fid_q, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-queued"))
        store.set_status(fid_q, "queued")
        fid_r, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-recheck"))
        store.set_status(fid_r, "rechecking")

        kind, target = pick_next(store, cfg)
        assert kind == "recheck"
        assert target["id"] == fid_r

    def test_queued_beats_repos(self, store: Store, cfg: Config) -> None:
        rid = store.add_repo("r", "https://r", "/nonexistent")
        fid_q, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid_q, "queued")

        kind, target = pick_next(store, cfg)
        assert kind == "fix"
        assert target["id"] == fid_q

    def test_oldest_queued_wins(self, store: Store, cfg: Config) -> None:
        rid = store.add_repo("r", "https://r", "/nonexistent")
        fid_old, _ = store.upsert_finding(rid, _make_finding(fingerprint="old"))
        store.set_status(fid_old, "queued")
        fid_new, _ = store.upsert_finding(rid, _make_finding(fingerprint="new"))
        store.set_status(fid_new, "queued")

        kind, target = pick_next(store, cfg)
        assert kind == "fix"
        assert target["id"] == fid_old  # DESC-ordered list, last = oldest

    def test_budget_override_jumps_the_queue_ahead_of_higher_priority_category(
        self, store: Store, cfg: Config
    ) -> None:
        """An overridden queued fix must win even though an attention item
        (normally higher priority) exists without an override."""
        rid = store.add_repo("r", "https://r", "/nonexistent")
        fid_a, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-attn"))
        store.set_status(fid_a, "pr_open")
        store.upsert_pr_state(fid_a, pr_number=1, needs_attention="new_comments", synced_at=1)
        fid_q, _ = store.upsert_finding(rid, _make_finding(fingerprint="fp-override"))
        store.set_status(fid_q, "queued")
        store.set_budget_override(fid_q, "once")

        kind, target = pick_next(store, cfg)
        assert kind == "fix"
        assert target["id"] == fid_q


class TestPickNextRepoRotation:
    def test_not_yet_cloned_picks_hunt(self, store: Store, cfg: Config) -> None:
        rid = store.add_repo("r", "https://r", "/definitely/not/cloned")
        kind, target = pick_next(store, cfg)
        assert kind == "hunt"
        assert target["id"] == rid

    def test_never_run_job_types_picked_by_priority_not_alphabetically(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        repo_path = tmp_path / "repo"
        repo_path.mkdir()
        rid = store.add_repo("r", "https://r", str(repo_path))
        store.set_last_hunt(rid, "deadbeef")  # hunt has run; others haven't

        kind, _target = pick_next(store, cfg)
        # never-run set is {test_gap, dep_update, refactor}; priority order
        # (hunt, test_gap, dep_update, refactor, modernization) picks
        # test_gap first -- NOT "dep_update", which is where alphabetical
        # sorting would have landed (and did, before this was fixed).
        assert kind == "test_gap"

    def test_denied_hunt_is_retried_before_a_never_run_sibling(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """Reproduces a real production incident (repo `recentIP`): hunt got
        denied for budget, leaving last_hunt_at at 0 (a denial never updates
        the watermark -- see run_hunt's deny path). The next cycle must
        retry hunt, not jump to dep_update just because "dep_update" sorts
        before "hunt" alphabetically and both are tied at 0."""
        repo_path = tmp_path / "repo"
        repo_path.mkdir()  # already cloned (a denied hunt still clones first)
        rid = store.add_repo("r", "https://r", str(repo_path))
        # hunt was attempted and denied -- last_hunt_at is still unset, exactly
        # like every other never-run job type for this brand-new repo.

        kind, target = pick_next(store, cfg)
        assert kind == "hunt"
        assert target["id"] == rid

    def test_oldest_last_run_wins_when_none_are_never_run(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        repo_path = tmp_path / "repo"
        repo_path.mkdir()
        rid = store.add_repo("r", "https://r", str(repo_path))
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (500, 400, 300, 100, 600, rid),  # refactor is oldest (100)
        )
        store.db.commit()

        kind, _target = pick_next(store, cfg)
        assert kind == "refactor"

    def test_least_recently_hunted_repo_wins(self, store: Store, cfg: Config) -> None:
        r1 = store.add_repo("r1", "https://r1", "/nonexistent1")
        r2 = store.add_repo("r2", "https://r2", "/nonexistent2")
        store.set_last_hunt(r1, "sha1")  # r1 has hunted before, r2 never has

        kind, target = pick_next(store, cfg)
        assert kind == "hunt"
        assert target["id"] == r2  # never-hunted beats already-hunted

    def test_force_repo_overrides_selection(self, store: Store, cfg: Config) -> None:
        r1 = store.add_repo("r1", "https://r1", "/nonexistent1")
        store.add_repo("r2", "https://r2", "/nonexistent2")

        kind, target = pick_next(store, cfg, force_repo="r1")
        assert kind == "hunt"
        assert target["id"] == r1

    def test_force_repo_unknown_raises(self, store: Store, cfg: Config) -> None:
        with pytest.raises(ValueError, match="unknown repo"):
            pick_next(store, cfg, force_repo="nonexistent-repo-name")


class TestPickNextModernizationGate:
    """modernization is a periodic strategic check (default 30-day interval),
    not a tight-loop scan -- it must not compete for scan slots against
    hunt/test_gap/dep_update/refactor every single cycle."""

    def test_never_run_modernization_is_eligible_immediately(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        repo_path = tmp_path / "repo"
        repo_path.mkdir()
        rid = store.add_repo("r", "https://r", str(repo_path))
        # Other four have all run; only modernization is never-run.
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=? WHERE id=?",
            (500, 400, 300, 100, rid),
        )
        store.db.commit()

        kind, _target = pick_next(store, cfg)
        assert kind == "modernization"

    def test_gated_out_within_interval_even_if_otherwise_stalest(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        repo_path = tmp_path / "repo"
        repo_path.mkdir()
        rid = store.add_repo("r", "https://r", str(repo_path))
        recent = now_ms()
        # modernization ran a moment ago (well inside the 30-day interval);
        # the other four ran long before that -- would lose to modernization
        # on staleness alone if the gate didn't exempt it.
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (100, 200, 300, 400, recent, rid),
        )
        store.db.commit()

        kind, _target = pick_next(store, cfg)
        assert kind == "hunt"  # oldest of the four; modernization gated out

    def test_eligible_again_once_interval_has_elapsed(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        repo_path = tmp_path / "repo"
        repo_path.mkdir()
        rid = store.add_repo("r", "https://r", str(repo_path))
        interval_ms = cfg.modernization_interval_days * 86_400_000
        long_ago = now_ms() - interval_ms - 1000
        # modernization last ran just over the interval ago -- it's the
        # stalest job now, and the gate no longer excludes it.
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (now_ms(), now_ms(), now_ms(), now_ms(), long_ago, rid),
        )
        store.db.commit()

        kind, _target = pick_next(store, cfg)
        assert kind == "modernization"
