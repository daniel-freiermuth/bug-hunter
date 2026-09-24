"""Tests for run_cycle's rotation-fairness guard: a repo-level job type
(test_gap/dep_update/refactor/modernization) that succeeded once and then
fails every subsequent attempt must not monopolize the repo's rotation
slot forever.

Before the fix, the guard only bumped last_{kind}_at when the job type
had NEVER run (timestamp still 0). Once a timestamp went non-zero (one
success), a persistent failure left it frozen there while siblings'
timestamps advanced past it on their own successes -- min()-based
fairness in pick_next then re-selected the stuck kind every cycle,
starving its siblings indefinitely.

The other edge of the same condition is a SUSPENSION. A cap kill is a
pause rather than a spent turn, and re-selecting that work belongs to
the resume tier, which claims the job by id above rotation -- so
'suspended' must stay out of the bump.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter import scheduler
from hunter.scheduler import pick_next, run_cycle
from hunter.store import Store
from hunter.types import Config, Row


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)


def _always_fails(
    _store: Store, _cfg: Config, _repo: Row, _backend: object, *, resume: object = None
) -> Row:
    return {"kind": "test_gap", "error": "simulated persistent failure"}


def _done_without_ingest(
    _store: Store, _cfg: Config, _repo: Row, _backend: object, *, resume: object = None
) -> Row:
    return {"kind": "test_gap", "state": "done"}


def _done_with_invalid_ingest(
    _store: Store, _cfg: Config, _repo: Row, _backend: object, *, resume: object = None
) -> Row:
    return {
        "kind": "test_gap",
        "state": "done",
        "ingest": {"inserted": 0, "duplicates": 0, "invalid": 1},
    }


def _killed_at_the_wall_clock(
    _store: Store, _cfg: Config, _repo: Row, _backend: object, *, resume: object = None
) -> Row:
    """A kill with no transcript to continue -- an ordinary spent turn."""
    return {"kind": "test_gap", "state": "killed"}


def _suspended_at_the_cap(
    store: Store, cfg: Config, repo: Row, _backend: object, *, resume: object = None
) -> Row:
    """A cap kill that left its transcript behind.

    Writes the job row a real executor would write, because the
    consequence being tested is about who re-selects this work: the
    resume tier claims a suspended row by id, and it can only do that if
    the row exists.
    """
    session = cfg.work_root / "session.jsonl"
    session.parent.mkdir(parents=True, exist_ok=True)
    session.write_text("{}\n")
    job = store.create_job("test_gap", repo["id"], state="running", estimated_tokens=30_000)
    store.update_job(
        job,
        state="suspended",
        tokens_new=30_000,
        killed_reason="cap",
        session_file=str(session),
        finished_at=scheduler.now_ms(),
    )
    return {"kind": "test_gap", "job": job, "state": "suspended"}


class TestJobRotationStarvation:
    def test_persistent_failure_does_not_monopolize_the_rotation(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        rid = store.add_repo("r", "https://r", tmp_path)
        repo_path = Path(store.get_repo(rid)["path"])
        repo_path.mkdir()
        # test_gap succeeded once, long ago -- now the stalest of the four,
        # so pick_next picks it first. dep_update/refactor are more recent
        # (already had their turn); hunt is freshest of all.
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (5000, 1000, 3000, 4000, scheduler.now_ms() + 999_999, rid),
        )
        store.db.commit()

        fake_runners = {**scheduler._RUNNERS, "test_gap": _always_fails}
        monkeypatch.setattr(scheduler, "_RUNNERS", fake_runners)

        result1 = run_cycle(store, cfg, backend=object())
        assert result1.get("kind") == "test_gap", result1

        after1 = store.get_repo(rid)
        assert after1 is not None
        assert after1["last_test_gap_at"] > 1000, (
            "a failed attempt must still bump last_test_gap_at, or this kind"
            " keeps winning min()-based fairness forever"
        )

        # Next cycle: test_gap's timestamp is now the freshest (~now), so
        # rotation must move on to dep_update (next-stalest at 3000), NOT
        # pick test_gap again. pick_next is pure/side-effect-free -- exactly
        # what's needed here, since the real dep_update runner would need a
        # genuine git checkout this fixture doesn't provide.
        kind2, _target2, _resume = pick_next(store, cfg)
        assert kind2 != "test_gap", (
            f"test_gap was re-selected again immediately after failing --"
            f" starvation bug reproduced: kind={kind2!r}"
        )
        assert kind2 == "dep_update", kind2

    def test_done_without_ingest_does_not_monopolize_the_rotation(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worker that finishes 'done' but produces no ingestable output
        (no 'ingest' key at all) must still bump last_test_gap_at -- this
        is the anti-starvation safety net, distinct from _run_analysis_job's
        own success-gated timestamp update which governs normal retry
        cadence for THIS kind."""
        rid = store.add_repo("r", "https://r", tmp_path)
        repo_path = Path(store.get_repo(rid)["path"])
        repo_path.mkdir()
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (5000, 1000, 3000, 4000, scheduler.now_ms() + 999_999, rid),
        )
        store.db.commit()

        fake_runners = {**scheduler._RUNNERS, "test_gap": _done_without_ingest}
        monkeypatch.setattr(scheduler, "_RUNNERS", fake_runners)

        result1 = run_cycle(store, cfg, backend=object())
        assert result1.get("kind") == "test_gap", result1

        after1 = store.get_repo(rid)
        assert after1 is not None
        assert after1["last_test_gap_at"] > 1000, (
            "done-without-output must still bump last_test_gap_at, or this"
            " kind keeps winning min()-based fairness forever"
        )

        kind2, _target2, _resume = pick_next(store, cfg)
        assert kind2 != "test_gap", (
            f"test_gap was re-selected again immediately after producing no"
            f" output -- starvation bug reproduced: kind={kind2!r}"
        )

    def test_invalid_ingestion_does_not_monopolize_the_rotation(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worker that finishes 'done' with output, but every candidate is
        rejected as invalid, must still bump last_test_gap_at for the same
        anti-starvation reason."""
        rid = store.add_repo("r", "https://r", tmp_path)
        repo_path = Path(store.get_repo(rid)["path"])
        repo_path.mkdir()
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (5000, 1000, 3000, 4000, scheduler.now_ms() + 999_999, rid),
        )
        store.db.commit()

        fake_runners = {**scheduler._RUNNERS, "test_gap": _done_with_invalid_ingest}
        monkeypatch.setattr(scheduler, "_RUNNERS", fake_runners)

        result1 = run_cycle(store, cfg, backend=object())
        assert result1.get("kind") == "test_gap", result1

        after1 = store.get_repo(rid)
        assert after1 is not None
        assert after1["last_test_gap_at"] > 1000, (
            "fully-invalid ingestion must still bump last_test_gap_at, or"
            " this kind keeps winning min()-based fairness forever"
        )

        kind2, _target2, _resume = pick_next(store, cfg)
        assert kind2 != "test_gap", (
            f"test_gap was re-selected again immediately after an invalid"
            f" ingestion -- starvation bug reproduced: kind={kind2!r}"
        )

    def test_killed_attempt_does_not_monopolize_the_rotation(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worker killed mid-run spent its turn, so last_test_gap_at must
        move. Nothing came of the attempt, but something ran and it is
        over -- leaving the timestamp stale would make this kind the
        stalest pick again next cycle, and the cycle after that."""
        rid = store.add_repo("r", "https://r", tmp_path)
        repo_path = Path(store.get_repo(rid)["path"])
        repo_path.mkdir()
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (5000, 1000, 3000, 4000, scheduler.now_ms() + 999_999, rid),
        )
        store.db.commit()

        fake_runners = {**scheduler._RUNNERS, "test_gap": _killed_at_the_wall_clock}
        monkeypatch.setattr(scheduler, "_RUNNERS", fake_runners)

        result1 = run_cycle(store, cfg, backend=object())
        assert result1.get("state") == "killed", result1

        after1 = store.get_repo(rid)
        assert after1 is not None
        assert after1["last_test_gap_at"] > 1000, (
            "a killed attempt must still bump last_test_gap_at, or this kind"
            " keeps winning min()-based fairness forever"
        )

        kind2, _target2, _resume = pick_next(store, cfg)
        assert kind2 == "dep_update", (
            f"after test_gap's turn the next-stalest kind must be selected: kind={kind2!r}"
        )

    def test_suspended_attempt_does_not_bump_the_rotation_timestamp(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A cap kill with a transcript is a pause, not a spent turn.

        The resume tier re-selects that work by id, above rotation, so
        the bump has no job left to do here -- and doing it anyway would
        record a turn the work never took.
        """
        rid = store.add_repo("r", "https://r", tmp_path)
        repo_path = Path(store.get_repo(rid)["path"])
        repo_path.mkdir()
        store.db.execute(
            "UPDATE repos SET last_hunt_at=?, last_test_gap_at=?,"
            " last_dep_update_at=?, last_refactor_at=?, last_modernization_at=? WHERE id=?",
            (5000, 1000, 3000, 4000, scheduler.now_ms() + 999_999, rid),
        )
        store.db.commit()

        fake_runners = {**scheduler._RUNNERS, "test_gap": _suspended_at_the_cap}
        monkeypatch.setattr(scheduler, "_RUNNERS", fake_runners)

        result1 = run_cycle(store, cfg, backend=object())
        assert result1.get("state") == "suspended", result1

        after1 = store.get_repo(rid)
        assert after1 is not None
        assert after1["last_test_gap_at"] == 1000, (
            "a suspension must leave last_test_gap_at alone: the scan was"
            " paused before it produced anything, so bumping it records a"
            " turn that was never taken. The resume tier owns re-selecting"
            " this work, so a bump would have the same job offered as a"
            " resume AND passed over by rotation, and the repo's record"
            " would claim a scan ran when none did"
            f" -- last_test_gap_at={after1['last_test_gap_at']}"
        )

        # The other half of the same contract: the work is not lost by
        # being left out of the bump, it is claimed one tier higher.
        kind2, _target2, resume2 = pick_next(store, cfg)
        assert resume2 is not None, (
            f"the resume tier must claim the suspension, got kind={kind2!r} with no resume plan"
        )
        assert kind2 == "test_gap", kind2
        assert resume2.predecessor_id == result1["job"]
