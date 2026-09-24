"""The resume policy: which killed jobs are a pause rather than a
failure, which suspension the next cycle continues, what it reserves for
it, and when it stops continuing at all.

The mechanism (schema link, resumable query, chain sum, --resume harness)
is covered elsewhere; these are the scheduler's decisions on top of it.
"""

from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from typing import Any

import pytest

from hunter.backend import Granted, Outlook
from hunter.scheduler import RESUME_PROMPT, _record_job, pick_next, run_cycle
from hunter.store import Store
from hunter.types import Config, RunResult, now_ms


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)


def _run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _make_upstream(tmp_path: Path) -> Path:
    """A bare upstream with one commit -- what a clone of the repo under
    test is made from."""
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


def _cloned_repo(store: Store, tmp_path: Path) -> int:
    """A repo row whose store-owned clone directory actually exists, so
    the resume tier sees a working directory to continue into."""
    upstream = _make_upstream(tmp_path)
    rid = store.add_repo("r", str(upstream), tmp_path, default_branch="main")
    dest = Path(store.get_repo(rid)["path"])  # type: ignore[index]
    _run("git", "clone", str(upstream), str(dest), cwd=tmp_path)
    return rid


def _bare_repo(store: Store, tmp_path: Path) -> int:
    """A repo row with nothing but an existing directory -- enough for
    selection, which only stats the path."""
    rid = store.add_repo("r", "https://r", tmp_path)
    Path(store.get_repo(rid)["path"]).mkdir(parents=True)  # type: ignore[index]
    return rid


def _ledger(path: Path, *contexts: tuple[int, int, int]) -> Path:
    """A worker transcript whose assistant calls carried the given
    (input, cacheRead, cacheWrite) triples, newest last."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(
            json.dumps(
                {
                    "message": {
                        "role": "assistant",
                        "usage": {
                            "input": i,
                            "output": 7,
                            "cacheRead": r,
                            "cacheWrite": w,
                        },
                    }
                }
            )
            + "\n"
            for i, r, w in contexts
        )
    )
    return path


def _typical_history(store: Store, repo_id: int, kind: str, cost: int, n: int = 3) -> None:
    """n completed jobs of one cost, so anticipated_tokens for `kind` is
    exactly that cost whatever percentile it picks."""
    for _ in range(n):
        jid = store.create_job(kind, repo_id)
        store.update_job(jid, state="done", tokens_new=cost, finished_at=now_ms())


def _suspend(store: Store, job_id: int, session_file: Path, tokens: int) -> None:
    store.update_job(
        job_id,
        state="suspended",
        killed_reason="cap",
        session_file=str(session_file),
        tokens_new=tokens,
        finished_at=now_ms(),
    )


class _FakeBackend:
    """Always grants; records what run() was asked to do and replays a
    canned RunResult (or one built from the prompt)."""

    def __init__(self, worker: Any) -> None:
        self._worker = worker
        self.prompts: list[str] = []
        self.resume_from: list[Path | None] = []
        self.anticipated: list[int] = []

    def decide(self, *, anticipated_tokens: int = 0) -> Outlook:
        self.anticipated.append(anticipated_tokens)
        granted = Granted(cap_tokens=200_000)
        return Outlook(normal=granted, prioritized=granted)

    def run(
        self,
        cwd: Path,
        prompt: str,
        *,
        cap_tokens: int | None,
        max_wall_s: float,
        job_class: object,
        resume_from: Path | None = None,
    ) -> RunResult:
        self.prompts.append(prompt)
        self.resume_from.append(resume_from)
        return self._worker(prompt)  # type: ignore[no-any-return]

    def keep_fresh(self) -> bool:
        return False

    def status(self) -> str:
        return ""


class TestSuspendedIsAPause:
    """A cap kill left its context on disk; a wallclock kill left a
    runaway. Only the first is worth continuing."""

    def test_a_cap_kill_with_a_session_file_becomes_suspended(self, store: Store) -> None:
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)

        state = _record_job(
            store,
            jid,
            RunResult(
                exit_code=None,
                killed_reason="cap",
                tokens_new=60_000,
                calls=12,
                session_file="/s/run/session.jsonl",
                duration_s=90.0,
                stdout_tail="halfway through",
            ),
        )

        assert state == "suspended"
        assert store.list_jobs()[0]["state"] == "suspended"

    def test_a_wallclock_kill_stays_killed(self, store: Store) -> None:
        """Same session file, same absent exit code -- only the reason
        differs, and an unbounded overrun is the one thing resuming would
        simply repeat."""
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)

        state = _record_job(
            store,
            jid,
            RunResult(
                exit_code=None,
                killed_reason="wallclock",
                tokens_new=60_000,
                calls=12,
                session_file="/s/run/session.jsonl",
                duration_s=1800.0,
                stdout_tail="still going",
            ),
        )

        assert state == "killed"
        assert store.list_jobs()[0]["state"] == "killed"

    def test_a_cap_kill_with_no_session_file_stays_killed(self, store: Store) -> None:
        """Nothing to hand --resume, so there is no pause to record."""
        rid = store.add_repo("r", "https://r", "/r")
        jid = store.create_job("hunt", rid)

        state = _record_job(
            store,
            jid,
            RunResult(
                exit_code=None,
                killed_reason="cap",
                tokens_new=60_000,
                calls=1,
                session_file=None,
                duration_s=90.0,
                stdout_tail="",
            ),
        )

        assert state == "killed"


class TestNoDoubleResume:
    def test_a_suspension_that_already_has_a_successor_is_not_offered_again(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """Both attempts would continue the same transcript in place, so
        the successor row is what marks the suspension as taken -- which
        is why no separate 'resumed' state exists."""
        rid = _bare_repo(store, tmp_path)
        _typical_history(store, rid, "hunt", 200_000)
        first = store.create_job("hunt", rid)
        _suspend(store, first, _ledger(tmp_path / "s" / "a.jsonl", (10, 90_000, 0)), 60_000)

        picked = pick_next(store, cfg)
        assert picked is not None
        assert picked[2] is not None, "the suspension is resumable before anything claims it"
        assert picked[2].predecessor_id == first

        successor = store.create_job("hunt", rid, state="running", resumed_from=first)

        assert store.list_resumable_jobs() == []
        again = pick_next(store, cfg)
        assert again is not None
        assert again[2] is None, f"job {first} was offered a second time while {successor} runs it"


class TestResumeReservation:
    """anticipated = ctx_at_suspension + max(typical - chain_spent, 25_000)."""

    def _suspended_hunt(
        self, store: Store, cfg: Config, tmp_path: Path, *, spent: int, ctx: tuple[int, int, int]
    ) -> int:
        rid = _bare_repo(store, tmp_path)
        _typical_history(store, rid, "hunt", 200_000)
        jid = store.create_job("hunt", rid)
        _suspend(store, jid, _ledger(tmp_path / "s" / "a.jsonl", (1, 1, 1), ctx), spent)
        return jid

    def test_reservation_is_context_carried_plus_what_is_left_of_a_typical_job(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """The last call's input + cacheRead + cacheWrite is what a resume
        re-caches; the remainder of the per-kind estimate is what the work
        still plausibly needs."""
        self._suspended_hunt(store, cfg, tmp_path, spent=60_000, ctx=(1_000, 120_000, 4_000))

        picked = pick_next(store, cfg)
        assert picked is not None
        plan = picked[2]
        assert plan is not None
        assert plan.anticipated == 125_000 + 140_000, (
            f"ctx 125000 + max(200000 - 60000, 25000); got {plan.anticipated}"
        )

    def test_the_floor_binds_once_the_chain_has_outspent_a_typical_job(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """A suspension has usually already spent more than typical -- that
        is why it ran out of headroom -- so the remainder term goes
        negative and must not be what is reserved."""
        self._suspended_hunt(store, cfg, tmp_path, spent=250_000, ctx=(1_000, 120_000, 4_000))

        picked = pick_next(store, cfg)
        assert picked is not None
        plan = picked[2]
        assert plan is not None
        assert plan.anticipated == 125_000 + 25_000, (
            f"ctx 125000 + the 25000 floor (200000 - 250000 is negative); got {plan.anticipated}"
        )

    def test_an_unreadable_transcript_falls_back_to_the_per_kind_estimate(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        rid = _bare_repo(store, tmp_path)
        _typical_history(store, rid, "hunt", 200_000)
        jid = store.create_job("hunt", rid)
        _suspend(store, jid, tmp_path / "s" / "gone.jsonl", 60_000)

        picked = pick_next(store, cfg)
        assert picked is not None
        plan = picked[2]
        assert plan is not None
        assert plan.anticipated == 200_000 + 140_000


class TestGiveUpCeiling:
    def test_a_chain_past_the_ceiling_is_retired_instead_of_continued(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """Three times the typical cost without finishing is not a job
        that is about to finish; continuing it is the loop this feature
        exists to end."""
        rid = _bare_repo(store, tmp_path)
        _typical_history(store, rid, "hunt", 200_000)
        jid = store.create_job("hunt", rid)
        _suspend(store, jid, _ledger(tmp_path / "s" / "a.jsonl", (1_000, 120_000, 4_000)), 700_000)

        picked = pick_next(store, cfg)

        assert picked is not None
        assert picked[2] is None, "a chain past 3x the typical cost must not be resumed"
        row = store.db.execute(
            "SELECT state, killed_reason FROM jobs WHERE id = ?", (jid,)
        ).fetchone()
        assert (row["state"], row["killed_reason"]) == ("failed", "give-up")
        assert store.list_resumable_jobs() == []
        event = next(e for e in store.recent_events(limit=20) if e["kind"] == "resume")
        assert "giving up after 700000 tok" in event["message"]
        assert "hunt r" in event["message"]


class TestResumeUnavailable:
    def test_the_attempt_fails_and_the_predecessor_is_retired(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """omp silently starts a FRESH session when --resume cannot be
        resolved, so "cannot resume" must not become "start cold here":
        the attempt is recorded failed, the predecessor is taken out of
        the resumable set, and nothing is spawned in its place."""
        rid = _cloned_repo(store, tmp_path)
        _typical_history(store, rid, "hunt", 200_000)
        predecessor = store.create_job("hunt", rid)
        _suspend(
            store,
            predecessor,
            _ledger(tmp_path / "s" / "a.jsonl", (1_000, 120_000, 4_000)),
            60_000,
        )

        backend = _FakeBackend(
            lambda _prompt: RunResult(
                exit_code=None,
                killed_reason="resume-unavailable",
                tokens_new=0,
                calls=0,
                session_file=None,
                duration_s=0.0,
                stdout_tail="resume source is gone",
            )
        )
        result = run_cycle(store, cfg, backend=backend)

        assert backend.resume_from == [Path(tmp_path / "s" / "a.jsonl")], (
            "the cycle must have asked for a continuation, not a cold run"
        )
        attempt = store.db.execute(
            "SELECT state FROM jobs WHERE id = ?", (result["job"],)
        ).fetchone()
        assert attempt["state"] == "failed"
        pred = store.db.execute("SELECT state FROM jobs WHERE id = ?", (predecessor,)).fetchone()
        assert pred["state"] == "killed", "a session omp cannot resolve is never resumable again"
        assert store.list_resumable_jobs() == []
        assert any("session gone" in e["message"] for e in store.recent_events(limit=20)), (
            "the lost session must be visible in the log, not just in a job row"
        )


class TestResumedRunIsTheSameWork:
    def test_a_resumed_hunt_continues_the_session_and_ingests_the_original_output(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """The resumed worker is still following the FIRST attempt's
        prompt, so it writes that job's findings file. Ingesting the new
        row's path instead would find nothing -- and because the watermark
        only advances on ingested output, the hunt would be redone from
        scratch forever."""
        rid = _cloned_repo(store, tmp_path)
        _typical_history(store, rid, "hunt", 200_000)
        predecessor = store.create_job("hunt", rid)
        _suspend(
            store,
            predecessor,
            _ledger(tmp_path / "s" / "a.jsonl", (1_000, 120_000, 4_000)),
            60_000,
        )
        # What the original prompt told the worker to write, and what the
        # continued session therefore still writes.
        original_out = cfg.work_root / "out" / f"job{predecessor}.findings.json"
        original_out.parent.mkdir(parents=True, exist_ok=True)

        def worker(prompt: str) -> RunResult:
            assert prompt == RESUME_PROMPT, "a resumed session must not be re-sent its full prompt"
            assert not re.search(r"Create (\S+) containing", prompt)
            original_out.write_text("[]")
            return RunResult(
                exit_code=0,
                killed_reason=None,
                tokens_new=40_000,
                calls=5,
                session_file=str(tmp_path / "s" / "a.jsonl"),
                duration_s=30.0,
                stdout_tail="done",
            )

        backend = _FakeBackend(worker)
        result = run_cycle(store, cfg, backend=backend)

        assert result["state"] == "done"
        assert result["resumed_from"] == predecessor
        assert result["ingest"] == {"inserted": 0, "duplicates": 0, "invalid": 0}
        assert store.get_repo(rid)["last_hunt_sha"] is not None, (  # type: ignore[index]
            "a completed resume must advance the watermark, or the work repeats"
        )
        attempt = store.db.execute(
            "SELECT resumed_from, estimated_tokens FROM jobs WHERE id = ?", (result["job"],)
        ).fetchone()
        assert attempt["resumed_from"] == predecessor
        assert attempt["estimated_tokens"] == 125_000 + 140_000, (
            "the budget must reserve the resume's own cost, not the cold per-kind estimate"
        )

    def test_a_resume_does_not_move_the_tree_under_the_running_session(
        self, store: Store, cfg: Config, tmp_path: Path
    ) -> None:
        """A cold hunt fast-forwards the clone first; a resumed one must
        not, because the transcript being continued describes the tree as
        it is now. Proven by taking the remote away: the fetch a cold run
        depends on would fail outright, and the resume must still run."""
        rid = _cloned_repo(store, tmp_path)
        clone = Path(store.get_repo(rid)["path"])  # type: ignore[index]
        _run("git", "remote", "remove", "origin", cwd=clone)
        _typical_history(store, rid, "hunt", 200_000)
        predecessor = store.create_job("hunt", rid)
        _suspend(
            store,
            predecessor,
            _ledger(tmp_path / "s" / "a.jsonl", (1_000, 120_000, 4_000)),
            60_000,
        )

        backend = _FakeBackend(
            lambda _prompt: RunResult(
                exit_code=0,
                killed_reason=None,
                tokens_new=40_000,
                calls=5,
                session_file=str(tmp_path / "s" / "a.jsonl"),
                duration_s=30.0,
                stdout_tail="done",
            )
        )
        result = run_cycle(store, cfg, backend=backend)

        assert "error" not in result, result
        assert result["resumed_from"] == predecessor
        assert backend.resume_from == [tmp_path / "s" / "a.jsonl"]
