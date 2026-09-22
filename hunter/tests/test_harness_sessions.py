"""Worker session directories: where they live, and that they are bounded.

The harness gives every run a private ``--session-dir`` because omp's
``autoResume`` otherwise continues the newest session for the same cwd, and
a resumed worker re-caches the whole prior transcript on its first call.
That fix trades one unbounded transcript for an unbounded *number* of
them, so the directories have to be both ours and pruned.
"""

from __future__ import annotations

import os
import time
from pathlib import Path

from hunter.backends.omp_scavenge import harness


def test_run_session_dir_lives_under_the_work_root(tmp_path: Path) -> None:
    """Not in the operator's ~/.omp/agent/sessions.

    A directory per run inside a tree a human also uses grows without
    bound in a place nothing else prunes.
    """
    run_dir = harness._run_session_dir(tmp_path, Path("/wr/repos/repo-1"), 1_700_000_000_000)

    assert run_dir.is_relative_to(harness.sessions_root(tmp_path))
    assert not run_dir.is_relative_to(Path.home() / ".omp")


def test_two_runs_in_one_worktree_get_separate_directories(tmp_path: Path) -> None:
    """Retries of a job share a cwd; sharing a directory would let the
    second resume the first, which is the whole bug."""
    cwd = Path("/wr/repos/repo-1")
    first = harness._run_session_dir(tmp_path, cwd, 1_700_000_000_000)
    second = harness._run_session_dir(tmp_path, cwd, 1_700_000_000_000)

    assert first != second


def test_prune_keeps_the_newest_and_is_idempotent(tmp_path: Path) -> None:
    root = harness.sessions_root(tmp_path)
    root.mkdir(parents=True)
    # Aged explicitly. Creation order is NOT reliable as age order:
    # filesystems differ in mtime resolution, and where the whole loop
    # lands inside one tick "the oldest ten" becomes an arbitrary ten.
    base = time.time() - 7200
    for i in range(harness.SESSIONS_RETAINED + 10):
        d = root / f"run-{i:03d}"
        d.mkdir()
        (d / "session.jsonl").write_text("{}")
        os.utime(d, (base + i * 60, base + i * 60))

    assert harness.prune_sessions(tmp_path) == 10

    left = {p.name for p in root.iterdir()}
    assert len(left) == harness.SESSIONS_RETAINED
    assert "run-059" in left, "the newest must survive"
    assert "run-000" not in left, "the oldest must go"

    # At the bound, a second pass removes nothing.
    assert harness.prune_sessions(tmp_path) == 0


def test_prune_is_a_noop_when_there_is_nothing_to_prune(tmp_path: Path) -> None:
    """Including before the first run, when the root does not exist yet:
    pruning must never be the reason a job fails to start."""
    assert harness.prune_sessions(tmp_path) == 0

    harness.sessions_root(tmp_path).mkdir(parents=True)
    assert harness.prune_sessions(tmp_path) == 0


class TestSessionDirNameLength:
    """A deep work_root must still produce a creatable directory.

    The slug is one path component and components cap at NAME_MAX (255
    on Linux), so without the bound mkdir fails with ENAMETOOLONG
    before omp starts and the worker is reported unmetered -- a path
    length surfacing as a budget failure.
    """

    def test_a_deep_cwd_yields_a_creatable_session_dir(self, tmp_path: Path) -> None:
        deep = Path("/") / ("x" * 80) / ("y" * 80) / ("z" * 80)

        run_dir = harness._run_session_dir(tmp_path, deep, 1_790_000_000_000)

        assert len(run_dir.name.encode()) <= 255, f"{len(run_dir.name)} bytes: {run_dir.name}"
        run_dir.mkdir(parents=True)

    def test_two_deep_worktrees_keep_distinct_session_dirs(self, tmp_path: Path) -> None:
        base = Path("/") / ("q" * 200) / "worktrees"

        a = harness._run_session_dir(tmp_path, base / "f1", 1)
        b = harness._run_session_dir(tmp_path, base / "f2", 1)

        assert a != b, "head-truncation would collide these"
