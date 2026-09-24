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
from hunter.types import Config


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


def test_the_worker_never_inherits_an_open_stdin(tmp_path: Path) -> None:
    """omp blocks on a piped stdin before the session exists.

    Given one, it prints "Reading prompt from piped stdin (waiting for
    EOF)" and waits, so the worker writes no ledger at all and is killed
    as unmetered after the grace period, having spent its whole
    wall-clock slot doing nothing.

    The daemon only has /dev/null on fd 0 today because its unit sets no
    StandardInput and systemd defaults to null -- inherited, not chosen.
    Run it from a supervisor or a wrapper that pipes stdin and every
    worker hangs. So the test gives ITS OWN stdin a pipe that never
    closes: without that, fd 0 is already /dev/null under pytest and the
    assertion would hold whether or not the harness closes anything.
    """
    omp = tmp_path / "fake-omp"
    record = tmp_path / "child-stdin"
    omp.write_text("#!/bin/sh\nreadlink /proc/$$/fd/0 > " + str(record) + "\nexit 0\n")
    omp.chmod(0o755)

    cfg = Config(
        work_root=tmp_path / "wr",
        db_path=tmp_path / "wr" / "t.db",
        omp_bin=str(omp),
    )
    cwd = tmp_path / "repo"
    cwd.mkdir()

    read_fd, write_fd = os.pipe()
    saved = os.dup(0)
    try:
        os.dup2(read_fd, 0)  # a pipe with no writer closing it
        harness.run_worker(cfg, cwd, "prompt", cap_tokens=1000, max_wall_s=5)
    finally:
        os.dup2(saved, 0)
        for fd in (saved, read_fd, write_fd):
            os.close(fd)

    assert record.read_text().strip() == "/dev/null", (
        "the worker must be handed /dev/null, not the daemon's stdin"
    )


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
