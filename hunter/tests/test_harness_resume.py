"""Continuing a suspended worker instead of restarting it from scratch.

A job killed for want of window headroom has already paid for everything it
did; restarting it pays a fresh session floor and then redoes that work.
Continuing it costs only the transcript re-cache.  The mechanism is omp's
``--resume=<path to the session jsonl>``, which was measured rather than
assumed (omp v18.2.6, 2026-09-24): it coexists with ``--session-dir``, it
continues non-interactively under ``-p``, and it -- not ``--session-dir`` --
decides where the continued transcript is written.

The measurement that matters for these tests is that a resumed run APPENDS to
the predecessor's ledger.  Everything already in that file was billed to the
predecessor's job row, so this attempt must be billed only for what it added.
"""

from __future__ import annotations

from pathlib import Path

from hunter.backends.omp_scavenge import harness
from hunter.types import Config


def _record(input_: int, output: int, cache_write: int) -> str:
    return (
        '{"message":{"role":"assistant","usage":'
        f'{{"input":{input_},"output":{output},"cacheWrite":{cache_write}}}}}}}'
    )


def _fake_omp(tmp_path: Path, name: str, records: list[tuple[int, int, int]]) -> Path:
    """An omp that appends `records` to the ledger it was pointed at.

    Target resolution mirrors what omp actually does: with ``--resume=<path>``
    the records go to that file and ``--session-dir`` does not move them;
    without one they go into the private session directory this run was
    handed.  Resolving both from argv is the point -- a worker that ignored
    the flags would write where nothing looks.
    """
    script = tmp_path / name
    lines = [
        "#!/bin/sh",
        f'printf "%s\\n" "$*" >> "{tmp_path / "argv.log"}"',
        'd=""',
        'r=""',
        'for a in "$@"; do',
        '  case "$a" in',
        '    --session-dir=*) d="${a#--session-dir=}" ;;',
        '    --resume=*) r="${a#--resume=}" ;;',
        "  esac",
        "done",
        'ledger="${r:-$d/s.jsonl}"',
    ]
    lines += [f"printf '%s\\n' '{_record(*rec)}' >> \"$ledger\"" for rec in records]
    lines.append("exit 0")
    script.write_text("\n".join(lines) + "\n")
    script.chmod(0o755)
    return script


def _cfg(tmp_path: Path, omp: Path) -> Config:
    return Config(
        work_root=tmp_path / "wr",
        db_path=tmp_path / "wr" / "t.db",
        omp_bin=str(omp),
        poll_s=0.02,
    )


def _argv(tmp_path: Path) -> list[str]:
    return (tmp_path / "argv.log").read_text().splitlines()


def test_a_run_without_a_resume_source_starts_cold(tmp_path: Path) -> None:
    """Without a resume source nothing changes: a private, empty session
    directory and no ``--resume``.

    This is the branch every job took before resume existed, and it is the
    one that costs a whole session floor if it silently stops being cold.
    The absent flag is the assertion: ``--resume`` pointing anywhere at all
    hands the worker a transcript to re-cache.
    """
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, "fake-omp", [(1000, 200, 50)]))

    rr = harness.run_worker(cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30)

    assert rr.killed_reason is None
    assert rr.tokens_new == 1250
    argv = _argv(tmp_path)[0]
    assert "--resume" not in argv, f"a cold run must name no session to continue: {argv}"
    assert f"--session-dir={harness.sessions_root(cfg.work_root)}" in argv


def test_a_resume_source_is_continued_by_explicit_path(tmp_path: Path) -> None:
    """A resume names the transcript to continue, by path, and runs in the
    directory that holds it.

    Both flags, and both pointing at the predecessor: omp writes the continued
    session at the ``--resume`` path and ``--session-dir`` does not move it, so
    a run directory anywhere else would simply be empty.  Naming the path is
    also what distinguishes this from autoResume, which continues whatever
    session is newest for the cwd.
    """
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, "fake-omp", [(1000, 200, 50)]))

    first = harness.run_worker(cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30)
    assert first.session_file is not None
    ledger = Path(first.session_file)

    second = harness.run_worker(
        cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30, resume_from=ledger
    )

    assert second.killed_reason is None, second.stdout_tail
    argv = _argv(tmp_path)[1]
    assert f"--resume={ledger}" in argv, argv
    assert f"--session-dir={ledger.parent}" in argv, (
        f"the resumed run belongs in the directory holding that transcript: {argv}"
    )
    assert second.session_file == str(ledger), (
        "a resumed attempt continues one transcript, it does not open another"
    )


def test_a_resumed_run_meters_only_what_this_attempt_added(tmp_path: Path) -> None:
    """The attribution invariant: a resumed attempt is billed for what IT
    added, never for the transcript it inherited.

    ``_ledger_dir_usage`` sums the whole run directory, and a resumed worker
    appends to the predecessor's file in that same directory -- so without a
    pre-spawn baseline every link of a resume chain re-bills its own history.
    That would double-count the window, and because tokens_new feeds the
    anticipated_tokens percentiles it would also inflate the estimate for
    every later job of the same kind.
    """
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, "fake-omp", [(1000, 200, 50)]))

    first = harness.run_worker(cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30)
    assert first.tokens_new == 1250
    assert first.session_file is not None
    ledger = Path(first.session_file)

    cfg.omp_bin = str(_fake_omp(tmp_path, "fake-omp-2", [(2000, 300, 100), (400, 50, 10)]))
    second = harness.run_worker(
        cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30, resume_from=ledger
    )

    assert second.tokens_new == 2860, (
        "(2000+300+100) + (400+50+10); the predecessor's 1250 is already billed to its own job row"
    )
    assert second.calls == 2, "two new calls, not the transcript's three"
    # The inherited history is still on disk -- the subtraction is an
    # attribution rule, not a truncation of the transcript.
    assert harness.ledger_usage(ledger) == (4110, 3)


def test_a_missing_resume_source_is_refused_rather_than_restarted(tmp_path: Path) -> None:
    """A resume source that is gone is refused, not quietly restarted.

    omp does not help here: an unresolvable ``--resume`` path makes it start a
    fresh session, write it at that path, and exit 0.  From the outside that is
    indistinguishable from a successful continuation, so the daemon would pay a
    full session floor and redo the work while believing it had resumed.
    Whether to start clean is the scheduler's call, so the harness hands the
    decision back instead of making it invisibly.
    """
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, "fake-omp", [(1000, 200, 50)]))
    gone = cfg.work_root / "sessions" / "pruned-away" / "s.jsonl"

    rr = harness.run_worker(
        cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30, resume_from=gone
    )

    assert rr.killed_reason == "resume-unavailable", (
        "the caller asked for a continuation and must be told it cannot happen"
    )
    assert rr.exit_code is None, "nothing was spawned"
    assert rr.tokens_new == 0
    assert not (tmp_path / "argv.log").exists(), (
        "a refused resume must not spend a session floor finding out"
    )


def test_a_resume_that_did_not_take_is_reported(tmp_path: Path) -> None:
    """A resume that silently did not take is reported.

    The source existed, so the pre-check passed, but omp began a new session
    over it anyway -- a corrupt or unparseable transcript does this.  Exit code
    and session path are identical to a real continuation; the opening record
    is not, because a continuation appends and leaves it alone.
    """
    cwd = tmp_path / "repo"
    cwd.mkdir()
    cfg = _cfg(tmp_path, _fake_omp(tmp_path, "fake-omp", [(1000, 200, 50)]))

    first = harness.run_worker(cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30)
    assert first.session_file is not None
    ledger = Path(first.session_file)

    # Truncate-and-write is exactly what omp does when it decides the named
    # session is not resumable.
    clobber = tmp_path / "fake-omp-clobber"
    clobber.write_text(
        "#!/bin/sh\n"
        'r=""\n'
        'for a in "$@"; do case "$a" in --resume=*) r="${a#--resume=}" ;; esac; done\n'
        f"printf '%s\\n' '{_record(900, 100, 0)}' > \"$r\"\n"
        "exit 0\n"
    )
    clobber.chmod(0o755)
    cfg.omp_bin = str(clobber)

    second = harness.run_worker(
        cfg, cwd, "prompt", cap_tokens=1_000_000, max_wall_s=30, resume_from=ledger
    )

    assert second.stdout_tail.startswith("[resume-lost]"), (
        f"a continuation that did not happen must not read as one: {second.stdout_tail!r}"
    )
    assert second.tokens_new == 1000, "still metered, just not continued"
