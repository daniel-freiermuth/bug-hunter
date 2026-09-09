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


def _always_fails(_store: Store, _cfg: Config, _repo: Row, _backend: object) -> Row:
    return {"kind": "test_gap", "error": "simulated persistent failure"}


class TestJobRotationStarvation:
    def test_persistent_failure_does_not_monopolize_the_rotation(
        self, store: Store, cfg: Config, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        repo_path = tmp_path / "repo"
        repo_path.mkdir()
        rid = store.add_repo("r", "https://r", str(repo_path))
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
        kind2, _target2 = pick_next(store, cfg)
        assert kind2 != "test_gap", (
            f"test_gap was re-selected again immediately after failing --"
            f" starvation bug reproduced: kind={kind2!r}"
        )
        assert kind2 == "dep_update", kind2
