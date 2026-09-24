"""Scheduler -- one cycle = one job. Fix work drains before new hunts;
run_cycle never raises (the loop that calls it must survive anything).
"""

from __future__ import annotations

import contextlib
import json
import logging
import re
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, Protocol

from .backend import Backend, Denied, Granted, JobClass

# The one place the scheduler reads a worker transcript rather than going
# through the Backend facade: the reservation for a resumed attempt needs
# the context size at the moment of suspension, which only the ledger
# holds. Kept as a plain read of the file format that wrote it rather than
# widening the Backend protocol for a single number.
from .backends.omp_scavenge.harness import ctx_at_suspension
from .forge import forge_for
from .ingest import ingest_findings
from .playbooks import (
    build_apply_improvement_prompt,
    build_apply_modernization_prompt,
    build_dep_update_prompt,
    build_engage_prompt,
    build_fix_prompt,
    build_harvest_prompt,
    build_hunt_prompt,
    build_modernization_prompt,
    build_recheck_prompt,
    build_refactor_prompt,
    build_test_gap_prompt,
)
from .store import Store
from .types import Config, Row, RunResult, now_ms
from .util import run_cmd

if TYPE_CHECKING:
    from collections.abc import Callable

log = logging.getLogger(__name__)
# The empty-tree SHA is the implicit parent of all root commits --
# used in diff_range for full-history re-hunts.
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"


# Below this many completed samples for a kind, _kind_token_history falls
# back to the unfiltered history. Three is the smallest count for which a
# percentile index picks anything other than an endpoint, and the fallback
# exists because the alternative is worse than a biased estimate: with no
# completed history at all the estimate is 0, and an anticipated of 0 makes
# the budget gate reserve nothing and wave through work it cannot fund.
_MIN_COMPLETED_SAMPLES = 3


def _kind_token_history(store: Store, kind: str) -> list[int]:
    """This kind's observed costs, ascending, from jobs that actually
    COMPLETED.

    Killed jobs are excluded because their tokens_new is not a measurement
    of what the work costs -- it is whatever bound killed them, so they
    cluster just above that bound and the estimator ends up measuring its
    own failures. Live data from when a fixed 200,000-token per-kind cap
    was still in force: 64 of 259 hunt jobs were killed, and including
    them put the cold p90 at 204,173 -- within 2% of the cap that did the
    killing -- against 73,724 for the 189 that finished. The per-kind cap
    is gone, but the ramp's headroom and the wall-clock limit still
    truncate jobs the same way.
    """
    sql = "SELECT tokens_new FROM jobs WHERE kind = ? AND tokens_new IS NOT NULL"
    done = [
        int(r["tokens_new"])
        for r in store.db.execute(
            sql + " AND state = 'done' ORDER BY tokens_new", (kind,)
        ).fetchall()
    ]
    if len(done) >= _MIN_COMPLETED_SAMPLES:
        return done
    return [
        int(r["tokens_new"])
        for r in store.db.execute(sql + " ORDER BY tokens_new", (kind,)).fetchall()
    ]


def anticipated_tokens(store: Store, cfg: Config, repo_id: int, kind: str) -> int:
    """Realistic anticipated cost of the job about to be decided on.

    This number IS the pre-start reservation: create_job writes it to
    jobs.estimated_tokens and store.running_estimate sums that column,
    so an under-estimate here is what lets rapid cycles overshoot a
    ramp. It has to be an estimate of the likely cost and not any kind
    of allowance, because a cold prompt-cache first call for a given
    (repo, kind) pair arrives as one atomic LLM call the harness cannot
    interrupt mid-flight (see harness.py) -- observed in production with
    dep_update/refactor jobs spending 2-4x what the scheduler had
    reserved for them, in a single call.

    If this exact (repo, kind) pair has finished within the configured
    cache TTL (cfg.cache_ttl_s, default 1h -- Anthropic's observed prompt-
    cache lifetime), its prompt cache is probably still warm -- anticipate
    its historical p90: a cold cache-write is likely, and a
    handful of jobs having been merely cheap doesn't mean this one will be.
    No history for this kind yet -> 0: nothing observed, nothing to
    reserve.
    """
    cache_ttl_ms = cfg.cache_ttl_s * 1000
    warm = (
        store.db.execute(
            "SELECT 1 FROM jobs WHERE repo_id = ? AND kind = ? AND finished_at > ?"
            " AND state != 'denied' LIMIT 1",
            (repo_id, kind, now_ms() - cache_ttl_ms),
        ).fetchone()
        is not None
    )
    history = _kind_token_history(store, kind)
    if not history:
        return 0
    idx = min(int(len(history) * (0.5 if warm else 0.9)), len(history) - 1)
    return int(history[idx])


# -- resume policy ----------------------------------------------------------


# What a resumed attempt is still allowed to expect to need, once its
# chain has already outspent the per-kind estimate. `z - chain_spent` is
# the honest "what is left of a typical job's budget", but a suspended
# attempt has usually already spent MORE than typical -- that is why it
# ran out of headroom -- which makes the term negative and meaningless.
# Floored here at enough budget for the worker to make real progress:
# anything smaller buys an attempt that re-caches its context and dies
# before doing any work, paying the whole re-cache for nothing.
MIN_PROGRESS_TOKENS = 25_000

# Multiple of the per-kind estimate at which a resume chain is abandoned
# rather than continued. A chain that has cost three times what this kind
# of job typically costs and still has not finished is not going to, and
# without a ceiling this tier would continue it forever -- the exact loop
# the feature exists to end, only now with a session file attached.
GIVE_UP_MULTIPLE = 3

# The entire prompt for a resumed attempt. The session already holds the
# original instructions and everything the worker concluded from them;
# re-sending the full prompt would only push that context further back
# while paying to re-cache it.
RESUME_PROMPT = (
    "Continue the work you were doing in this session."
    " You were interrupted; pick up where you left off."
)

# Ancestry hops _resume_origin will walk. Mirrors store's own chain cap
# for the same reason: the write path cannot produce a cycle (resumed_from
# is set once at INSERT to an id that already exists), but a hand-repaired
# or partially restored database must not be able to hang the scheduler.
_RESUME_ORIGIN_MAX_DEPTH = 64

# Kinds whose target is a finding row rather than a repo row, and the
# worktree each parks its worker in (see run_fix/run_engage/run_harvest).
# recheck is the odd one out: it is finding-driven but runs in the clone.
_FINDING_KINDS = ("engage", "harvest", "recheck", "fix")
_WORKTREE_PREFIX = {"fix": "f", "engage": "e", "harvest": "h"}


@dataclass(frozen=True)
class ResumePlan:
    """Everything an executor needs to continue a suspended attempt
    instead of starting one cold.

    Built by pick_next -- the policy lives with the selection it drives --
    and threaded into the ordinary run_* body, so a resumed attempt
    ingests findings, advances watermarks and ships PRs through exactly
    the same code as a cold one.

    `origin_job_id` is the job whose prompt the worker is still following
    (see _resume_origin); `ctx`/`typical`/`chain_spent` are the three
    measurements `anticipated` was computed from, kept so the event log
    can show the reservation's arithmetic rather than just its result.
    """

    predecessor_id: int
    session_file: Path
    origin_job_id: int
    repo_name: str
    anticipated: int
    ctx: int
    typical: int
    chain_spent: int


def _job_state(rr: RunResult) -> str:
    """The row state an outcome earns.

    A cap kill that left a session file behind is a PAUSE, not a failure:
    the worker ran out of window headroom with its context intact on
    disk. Continuing it costs the context it was carrying (across 112
    production re-cache events the re-cache / prior-context ratio had
    median 1.00 and p10 1.00), where restarting it pays a flat ~37,000
    token session floor -- measured as call #1 cacheWrite even in a
    4,255-call session -- before doing any work at all. With no session
    file there is nothing to continue, so it stays an ordinary kill.

    Every other killed_reason stays 'killed' and is never resumed. That
    is aimed squarely at wallclock: an unbounded overrun is the runaway
    signature, and continuing the session that produced it would only
    reproduce it with the clock reset.

    'resume-unavailable' is not an outcome of the work at all -- nothing
    was spawned (see harness._resume_unavailable) -- so the attempt is
    recorded as failed rather than killed.
    """
    if rr.killed_reason == "cap" and rr.session_file:
        return "suspended"
    if rr.killed_reason == "resume-unavailable":
        return "failed"
    if rr.killed_reason:
        return "killed"
    return "done" if rr.exit_code == 0 else "failed"


def _retire_lost_predecessor(store: Store, job_id: int) -> None:
    """A resume omp could not honour: retire the predecessor instead of
    offering it again.

    The predecessor becomes 'killed', not 'suspended': its session is
    gone, so every later cycle would make the identical failed attempt.
    Nothing is started cold in its place this cycle either -- omp silently
    starts a FRESH session when a --resume path will not resolve, so
    treating "cannot resume" as "start cold right here" would pay a full
    session floor while believing it had continued. The work is not lost:
    with the suspension retired, the next cycle selects it through the
    ordinary priorities and pays the cold price knowingly.
    """
    row = store.db.execute(
        "SELECT j.resumed_from, j.kind, r.name AS repo_name FROM jobs j"
        " JOIN repos r ON r.id = j.repo_id WHERE j.id = ?",
        (job_id,),
    ).fetchone()
    if row is None or row["resumed_from"] is None:
        return
    predecessor = int(row["resumed_from"])
    store.update_job(predecessor, state="killed")
    store.log_event(
        "error",
        f"resume {row['kind']} {row['repo_name']}: session gone,"
        f" predecessor job {predecessor} marked killed",
        job_id=job_id,
    )


def _record_job(
    store: Store,
    job_id: int,
    rr: RunResult,
    *,
    model: str | None = None,
) -> str:
    state = _job_state(rr)
    notes = rr.stdout_tail[-500:] if state != "done" and rr.stdout_tail else None
    store.update_job(
        job_id,
        state=state,
        pid=None,
        tokens_new=rr.tokens_new,
        calls=rr.calls,
        exit_code=rr.exit_code,
        killed_reason=rr.killed_reason,
        session_file=rr.session_file,
        model=model,
        usage_delta=rr.usage_delta,
        notes=notes,
        finished_at=now_ms(),
    )
    if rr.killed_reason == "resume-unavailable":
        # Handled here rather than in each executor: every kind reaches
        # this one function, and the row just written is the only place
        # that knows which suspension this attempt was continuing.
        _retire_lost_predecessor(store, job_id)
    return state


def _ingest_followups(
    store: Store, repo_id: int, worktree: Path, fid: int, job: int, kind: str
) -> None:
    """Read worktree/FOLLOW-UPS.json (if the worker wrote one) and file its
    entries as real findings -- each entry declares its own "type", since
    a follow-up can be anything (a deferred migration, a dep bump that's
    now achievable again, a test gap noticed along the way). Without this,
    a worker's "this is worth doing later" note lives only as prose in a
    PR/comment that stops being read the moment the PR closes or merges --
    see apply_improvement.md step 5 and engage.md's superseded-PR guidance
    for the two places a worker is told to write this file. Call BEFORE
    the caller drops the worktree. `kind` is the CALLER's event kind
    (engage/harvest), not a fixed value -- so the audit log correctly
    attributes which job actually filed the follow-up.
    """
    followups_path = worktree / "FOLLOW-UPS.json"
    if not followups_path.exists():
        return
    counts = ingest_findings(store, repo_id, followups_path, finding_type=None, job=job)
    if counts["inserted"]:
        store.log_event(
            kind,
            f"#{fid}: +{counts['inserted']} follow-up(s) filed from deferred/superseded work"
            f" ({counts['duplicates']} dup / {counts['invalid']} invalid)",
            job_id=job,
            finding_id=fid,
        )


# -- hunt -------------------------------------------------------------------


def run_hunt(
    store: Store,
    cfg: Config,
    repo: Row,
    backend: Backend,
    force: bool = False,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    rid: int = repo["id"]
    rname: str = repo["name"]
    rpath = Path(repo["path"])
    db: str = repo["default_branch"]

    # Ensure clone + fast-forward to origin's default branch -- skipped
    # whole for a resume: the worker is mid-hunt over the tree its
    # transcript describes, so fast-forwarding would move the ground under
    # a live session and then let the watermark below claim commits nobody
    # read. The clone is known to exist: pick_next refuses to offer a
    # suspension whose working directory is gone.
    if resume is None:
        if not rpath.exists():
            rpath.parent.mkdir(parents=True, exist_ok=True)
            # `--` before the operands: even a URL that slipped past
            # validation cannot be read as an option here.
            rc, out = run_cmd(["git", "clone", "--", repo["url"], str(rpath)], timeout=600)
            if rc != 0:
                store.log_event("error", f"hunt {rname}: clone failed: {out[-300:]}")
                return {"error": f"clone failed: {out[-300:]}"}
        for cmd in (
            ["git", "fetch", "origin"],
            ["git", "checkout", db],
            ["git", "pull", "--ff-only"],
        ):
            rc, out = run_cmd(["git", "-C", str(rpath), *cmd[1:]], timeout=600)
            if rc != 0:
                store.log_event(
                    "error",
                    f"hunt {rname}: {' '.join(cmd)} failed: {out[-300:]}",
                )
                return {"error": f"{' '.join(cmd)} failed: {out[-300:]}"}

    rc, head = run_cmd(["git", "-C", str(rpath), "rev-parse", "HEAD"])
    if rc != 0 or not head:
        store.log_event(
            "error",
            f"hunt {rname}: rev-parse HEAD failed: {head[-300:]}",
        )
        return {"error": "rev-parse HEAD failed"}

    # Check if full re-hunt is due (revisit old code periodically)
    last: str | None = repo.get("last_hunt_sha")
    last_full = repo.get("last_full_hunt_at")
    rehunt_interval_ms = cfg.hunt_rehunt_days * 86400_000
    # Only trigger re-hunt if we've completed at least one hunt before
    rehunt_due = last_full and (now_ms() - last_full) > rehunt_interval_ms
    full_rehunt_triggered = False

    # Both of the next two branches are cold-attempt business and are
    # suppressed for a resume: clearing the watermark mid-chain would
    # re-scope a hunt that is already in flight, and "no new commits" is
    # about whether to START work, not about work already half done.
    if rehunt_due and not force and resume is None:
        # Clear watermark → triggers full-history hunt below
        store.db.execute(
            "UPDATE repos SET last_hunt_sha = NULL WHERE id = ?",
            (rid,),
        )
        store.db.commit()
        store.log_event(
            "hunt",
            f"{rname}: full re-hunt triggered ({cfg.hunt_rehunt_days}d interval)",
        )
        last = None  # Force full hunt
        full_rehunt_triggered = True

    if last == head and not force and resume is None:
        # No new commits — update timestamp so scheduler rotates to next repo.
        store.set_last_hunt(repo["id"], head)
        store.log_event(
            "hunt",
            f"{rname}: no new commits since {head[:12]} -- skipped",
        )
        return {"skipped": "no new commits", "head": head}

    if last:
        diff_range = f"{last}..{head}"
        scope_note = f"Commits since the last completed hunt ({last[:12]})."
    elif full_rehunt_triggered:
        # Full re-hunt: scan from repository root
        rc, roots = run_cmd(
            [
                "git",
                "-C",
                str(rpath),
                "rev-list",
                "--max-parents=0",
                head,
            ]
        )
        if rc != 0 or not roots:
            store.log_event("error", f"hunt {rname}: failed to find repository root")
            return {"error": "failed to find repository root"}

        root = roots.splitlines()[-1]
        # Use git's empty tree to include the root commit itself
        # The empty tree SHA is the implicit parent of all root commits
        diff_range = f"{EMPTY_TREE}..{head}"
        scope_note = f"Periodic full re-hunt: complete history including root {root[:12]}."
    else:
        # First hunt: ~3 weeks or 30 commits
        rc, base = run_cmd(
            [
                "git",
                "-C",
                str(rpath),
                "rev-list",
                "-1",
                "--before=3 weeks ago",
                head,
            ]
        )
        scope = "the last ~3 weeks of commits"
        if not base or base == head:
            # Quiet repo: a time window is empty -- take the last 30 commits.
            rc, base = run_cmd(
                [
                    "git",
                    "-C",
                    str(rpath),
                    "rev-parse",
                    f"{head}~30",
                ]
            )
            scope = "the last 30 commits (repo quiet for >3 weeks)"
            if rc != 0 or not base:
                rc, roots = run_cmd(
                    [
                        "git",
                        "-C",
                        str(rpath),
                        "rev-list",
                        "--max-parents=0",
                        head,
                    ]
                )
                base = roots.splitlines()[-1] if roots else head
                scope = "the full history (small repo)"
        diff_range = f"{base}..{head}"
        scope_note = f"First hunt for this repo: {scope} (base {base[:12]})."

    anticipated = (
        resume.anticipated if resume is not None else anticipated_tokens(store, cfg, rid, "hunt")
    )
    outlook = backend.decide(anticipated_tokens=anticipated)
    verdict = outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            store.log_event("deny", f"hunt {rname}: {reason}")
            return {"denied": reason, "retry_at": retry_at}
        case Granted(cap_tokens=cap):
            pass
    try:
        job = store.create_job(
            "hunt",
            rid,
            cap_tokens=cap,
            state="running",
            estimated_tokens=anticipated,
            resumed_from=resume.predecessor_id if resume is not None else None,
        )
    except ValueError as e:
        # The repo was deleted after this run was picked: skip, don't crash.
        return {"skipped": str(e)}
    # A resumed worker writes where the ORIGINAL prompt told it to, so the
    # findings file to ingest belongs to the chain's first job, not to
    # this row (see _resume_origin).
    out_job = resume.origin_job_id if resume is not None else job
    out_path = cfg.work_root / "out" / f"job{out_job}.findings.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    prompt = (
        RESUME_PROMPT
        if resume is not None
        else build_hunt_prompt(
            repo,
            diff_range,
            scope_note,
            store.suppressions(rid),
            store.known_active(rid),
            out_path,
            cfg.hunt_max_findings,
            store.repo_notes(rid),
        )
    )
    model = cfg.model_for("hunt")
    rr = backend.run(
        rpath,
        prompt,
        cap_tokens=cap,
        max_wall_s=cfg.hunt_max_wall_s,
        job_class=JobClass.HUNT,
        resume_from=resume.session_file if resume is not None else None,
    )
    state = _record_job(store, job, rr, model=model)

    summary: Row = {
        "kind": "hunt",
        "repo": rname,
        "job": job,
        "state": state,
        "diff_range": diff_range,
        "tokens_new": rr.tokens_new,
        "head": head,
        "full_rehunt": full_rehunt_triggered,
    }
    if resume is not None:
        summary["resumed_from"] = resume.predecessor_id
    if out_path.exists():
        counts = ingest_findings(store, rid, out_path, job=job)
        summary["ingest"] = counts
        store.log_event(
            "hunt",
            f"{rname}: job {job} {state} over {diff_range[:25]}..."
            f" +{counts['inserted']} new / {counts['duplicates']} dup"
            f" / {counts['invalid']} invalid ({rr.tokens_new} tok)",
            job_id=job,
        )
        # Only update watermarks when output was successfully ingested
        if state == "done" and counts.get("invalid", 0) == 0:
            store.set_last_hunt(rid, head)
            if full_rehunt_triggered:
                store.db.execute(
                    "UPDATE repos SET last_full_hunt_at = ? WHERE id = ?",
                    (now_ms(), rid),
                )
                store.db.commit()
            elif last_full is None:
                # Seed the periodic-full-rehunt clock on this repo's first
                # completed hunt (whatever its scope). Without this,
                # last_full_hunt_at stays NULL forever: rehunt_due requires
                # it non-null, but the only other writer is gated behind
                # rehunt_due itself -- hunt_rehunt_days would never fire.
                store.db.execute(
                    "UPDATE repos SET last_full_hunt_at = ? WHERE id = ?",
                    (now_ms(), rid),
                )
                store.db.commit()
    else:
        store.log_event(
            "hunt",
            f"{rname}: job {job} {state}, no findings file ({rr.tokens_new} tok)",
            job_id=job,
        )

    return summary


# -- recheck ----------------------------------------------------------------


def run_recheck(
    store: Store,
    cfg: Config,
    finding: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    """Re-evaluate a finding against the current codebase. Human-triggered."""
    fid: int = finding["id"]
    if finding["status"] != "rechecking":
        return {
            "skipped": (f"finding #{fid} is {finding['status']!r}, not 'rechecking'"),
        }
    repo = store.get_repo(finding["repo_id"])
    if repo is None:
        store.log_event(
            "error",
            f"recheck #{fid}: repo {finding['repo_id']} missing",
            finding_id=fid,
        )
        return {"error": "repo missing"}
    rpath = Path(repo["path"])
    db: str = repo["default_branch"]

    # Ensure clone + fast-forward to latest default branch. A resume skips
    # it: the worker is already reasoning about the tree its transcript
    # describes, and moving that tree under a live session invalidates
    # every line number it has read.
    if resume is None:
        if not rpath.exists():
            rpath.parent.mkdir(parents=True, exist_ok=True)
            # `--` before the operands: even a URL that slipped past
            # validation cannot be read as an option here.
            rc, out = run_cmd(["git", "clone", "--", repo["url"], str(rpath)], timeout=600)
            if rc != 0:
                store.log_event(
                    "error",
                    f"recheck #{fid}: clone failed: {out[-300:]}",
                    finding_id=fid,
                )
                return {"error": f"clone failed: {out[-300:]}"}
        for cmd in (
            ["git", "fetch", "origin"],
            ["git", "checkout", db],
            ["git", "pull", "--ff-only"],
        ):
            rc, out = run_cmd(["git", "-C", str(rpath), *cmd[1:]], timeout=600)
            if rc != 0:
                store.log_event(
                    "error",
                    f"recheck #{fid}: {' '.join(cmd)} failed: {out[-300:]}",
                    finding_id=fid,
                )
                return {"error": f"{' '.join(cmd)} failed: {out[-300:]}"}

    # Budget gate -- recheck is investigative, like hunt.
    override = finding.get("budget_override")
    anticipated = (
        resume.anticipated
        if resume is not None
        else anticipated_tokens(store, cfg, repo["id"], "recheck")
    )
    outlook = backend.decide(anticipated_tokens=anticipated)
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            store.log_event("deny", f"recheck #{fid}: {reason}", finding_id=fid)
            return {"denied": reason, "retry_at": retry_at}
        case Granted(cap_tokens=cap):
            pass
    try:
        job = store.create_job(
            "recheck",
            repo["id"],
            finding_id=fid,
            cap_tokens=cap,
            state="running",
            estimated_tokens=anticipated,
            resumed_from=resume.predecessor_id if resume is not None else None,
        )
    except ValueError as e:
        # The repo was deleted after this run was picked: skip, don't crash.
        return {"skipped": str(e)}
    # Finding-keyed, so a resumed attempt writes the same path its own
    # prompt named -- no chain-origin lookup needed here.
    out_path = cfg.work_root / "out" / f"recheck{fid}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    if out_path.exists() and resume is None:
        # Only a cold attempt clears the file: any verdict sitting there
        # during a resume was written by this same chain (the cold attempt
        # that started it deleted whatever preceded it), so it is this
        # run's own partial output rather than a stale one.
        out_path.unlink()
    prompt = (
        RESUME_PROMPT
        if resume is not None
        else build_recheck_prompt(finding, repo, out_path, store.repo_notes(repo["id"]))
    )
    model = cfg.model_for("hunt")
    rr = backend.run(
        rpath,
        prompt,
        cap_tokens=cap,
        max_wall_s=cfg.hunt_max_wall_s,
        job_class=JobClass.HUNT,
        resume_from=resume.session_file if resume is not None else None,
    )
    state = _record_job(store, job, rr, model=model)
    summary: Row = {
        "kind": "recheck",
        "finding": fid,
        "job": job,
        "state": state,
        "tokens_new": rr.tokens_new,
    }

    # Post-process verdict file.
    raw_verdict: Row | None = None
    if state == "done" and out_path.exists():
        try:
            raw_verdict = json.loads(out_path.read_text())
        except (OSError, json.JSONDecodeError):
            raw_verdict = None

    if not isinstance(raw_verdict, dict) or raw_verdict.get("verdict") not in (
        "confirmed",
        "stale",
        "invalid",
    ):
        if state != "done":
            failure = f"worker {state}"
        elif raw_verdict is None and not out_path.exists():
            failure = "no verdict file"
        elif raw_verdict is None:
            failure = "unparseable verdict file"
        else:
            failure = "invalid verdict value"
        streak = store.record_recheck_attempt(fid, failure)
        if streak >= MAX_CONSECUTIVE_SAME_FAILURE:
            # Same recheck failure N times running: whatever's wrong isn't
            # going to resolve itself by retrying identically forever, and
            # a stuck 'rechecking' item head-of-line-blocks fix/rotation
            # work behind it (see pick_next's priority ladder). Fall back
            # to the inbox rather than pretend the recheck is still live.
            store.set_status(fid, "new")
            store.clear_recheck_attempts(fid)
            store.log_event(
                "recheck",
                f"#{fid} gave up after {streak} identical failures ({failure});"
                " back to inbox for human triage",
                job_id=job,
                finding_id=fid,
            )
            summary["outcome"] = "stuck"
            if override == "once":
                store.set_budget_override(fid, None)
            return summary
        # Leave status at 'rechecking' (don't reset to 'new') -- killed/failed/
        # inconclusive attempts must not look identical to "still relevant",
        # and the next cycle's priority scan naturally retries 'rechecking'
        # findings, same as a killed fix stays 'queued'.
        store.log_event(
            "recheck",
            f"#{fid}: job {job} {state}, verdict file missing/unparseable -- will retry",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "requeued"
        if override == "once":
            store.set_budget_override(fid, None)
        return summary

    v: str = raw_verdict["verdict"]
    reason = (raw_verdict.get("reason") or "")[:500]

    if v == "confirmed":
        store.update_finding_analysis(
            fid,
            summary=raw_verdict.get("updated_summary"),
            detail=raw_verdict.get("updated_detail"),
            confidence=raw_verdict.get("updated_confidence"),
            severity=raw_verdict.get("updated_severity"),
        )
        store.set_status(fid, "new")  # back to inbox with improved analysis
        store.clear_recheck_attempts(fid)
        store.log_event(
            "recheck",
            f"#{fid} confirmed: {reason}",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "confirmed"
    elif v == "stale":
        store.set_status(fid, "wontfix", verdict_reason=f"recheck: {reason}")
        store.clear_recheck_attempts(fid)
        store.log_event(
            "recheck",
            f"#{fid} stale: {reason}",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "stale"
    elif v == "invalid":
        store.set_status(fid, "rejected", verdict_reason=f"recheck: {reason}")
        store.clear_recheck_attempts(fid)
        store.log_event(
            "recheck",
            f"#{fid} invalid: {reason}",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "invalid"

    summary["verdict"] = v
    summary["reason"] = reason
    if override == "once":
        store.set_budget_override(fid, None)
    return summary


# -- analysis jobs (test gap / dep update / refactor / modernization) -------


@dataclass(frozen=True)
class _AnalysisSpec:
    """Static per-kind wiring for _run_analysis_job -- the only things that
    differ between test_gap/dep_update/refactor/modernization."""

    kind: str
    out_plural: str  # output filename plural, e.g. job{N}.<out_plural>.json
    no_output_noun: str  # "no <noun> file" log wording when out_path is missing
    scope_note: str
    prompt_builder: Callable[[Row, str, list[Row], list[Row], Path, int, str], str]


def _run_analysis_job(
    store: Store,
    cfg: Config,
    repo: Row,
    spec: _AnalysisSpec,
    backend: Backend,
    resume: ResumePlan | None = None,
) -> Row:
    """Shared body for the four repo-level analysis job types: sync the repo
    to its default branch, budget-gate, run the worker, ingest output, and
    advance the rotation timestamp on success. The only per-kind variation
    is which prompt builder runs and where the output/timestamp land.
    """
    rid: int = repo["id"]
    rname: str = repo["name"]
    rpath = Path(repo["path"])
    kind = spec.kind

    if not rpath.exists():
        store.log_event("error", f"{kind} {rname}: repo not cloned")
        return {"error": "repo not cloned"}

    # A resume does not sync: the worker is mid-scan over the tree its
    # transcript describes, and fast-forwarding it would move that tree
    # under a live session.
    if resume is None:
        for cmd in (
            ["git", "fetch", "origin"],
            ["git", "checkout", repo["default_branch"]],
            ["git", "pull", "--ff-only"],
        ):
            rc, out = run_cmd(["git", "-C", str(rpath), *cmd[1:]], timeout=600)
            if rc != 0:
                store.log_event("error", f"{kind} {rname}: {' '.join(cmd)} failed: {out[-300:]}")
                return {"error": f"{' '.join(cmd)} failed"}

    # Budget check -- analysis jobs share the hunt budget/model for now
    anticipated = (
        resume.anticipated if resume is not None else anticipated_tokens(store, cfg, rid, kind)
    )
    outlook = backend.decide(anticipated_tokens=anticipated)
    verdict = outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            store.log_event("deny", f"{kind} {rname}: {reason}")
            return {"denied": reason, "retry_at": retry_at}
        case Granted(cap_tokens=cap):
            pass
    try:
        job = store.create_job(
            kind,
            rid,
            cap_tokens=cap,
            state="running",
            estimated_tokens=anticipated,
            resumed_from=resume.predecessor_id if resume is not None else None,
        )
    except ValueError as e:
        # The repo was deleted after this run was picked: skip, don't crash.
        return {"skipped": str(e)}
    # A resumed worker writes where the ORIGINAL prompt told it to, so the
    # file to ingest belongs to the chain's first job (see _resume_origin).
    out_job = resume.origin_job_id if resume is not None else job
    out_path = cfg.work_root / "out" / f"job{out_job}.{spec.out_plural}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)

    prompt = (
        RESUME_PROMPT
        if resume is not None
        else spec.prompt_builder(
            repo,
            spec.scope_note,
            store.suppressions(rid, finding_type=kind),
            store.known_active(rid, finding_type=kind),
            out_path,
            cfg.hunt_max_findings,  # reuse hunt max for now
            store.repo_notes(rid),
        )
    )

    model = cfg.model_for("hunt")
    rr = backend.run(
        rpath,
        prompt,
        cap_tokens=cap,
        max_wall_s=cfg.hunt_max_wall_s,
        job_class=JobClass.HUNT,
        resume_from=resume.session_file if resume is not None else None,
    )
    state = _record_job(store, job, rr, model=model)

    summary: Row = {
        "kind": kind,
        "repo": rname,
        "job": job,
        "state": state,
        "tokens_new": rr.tokens_new,
    }
    if resume is not None:
        summary["resumed_from"] = resume.predecessor_id

    if out_path.exists():
        counts = ingest_findings(store, rid, out_path, finding_type=kind, job=job)
        summary["ingest"] = counts
        store.log_event(
            kind,
            f"{rname}: job {job} {state} -- +{counts['inserted']} new / {counts['duplicates']} dup"
            f" / {counts['invalid']} invalid ({rr.tokens_new} tok)",
            job_id=job,
        )
        # Only update timestamp after successful output + ingestion
        if state == "done" and counts.get("invalid", 0) == 0:
            sql = f"UPDATE repos SET last_{kind}_at = ? WHERE id = ?"  # noqa: S608
            store.db.execute(sql, (now_ms(), rid))
            store.db.commit()
    else:
        store.log_event(
            kind,
            f"{rname}: job {job} {state}, no {spec.no_output_noun} file ({rr.tokens_new} tok)",
            job_id=job,
        )

    return summary


_TEST_GAP_SPEC = _AnalysisSpec(
    kind="test_gap",
    out_plural="test_gaps",
    no_output_noun="gaps",
    scope_note="Full repository scan for test coverage gaps.",
    prompt_builder=build_test_gap_prompt,
)
_DEP_UPDATE_SPEC = _AnalysisSpec(
    kind="dep_update",
    out_plural="dep_updates",
    no_output_noun="updates",
    scope_note="Check all package manifests for outdated dependencies.",
    prompt_builder=build_dep_update_prompt,
)
_REFACTOR_SPEC = _AnalysisSpec(
    kind="refactor",
    out_plural="refactorings",
    no_output_noun="refactorings",
    scope_note=(
        "Scan for safe, mechanical refactoring opportunities (duplication, dead code, complexity)."
    ),
    prompt_builder=build_refactor_prompt,
)
_MODERNIZATION_SPEC = _AnalysisSpec(
    kind="modernization",
    out_plural="modernizations",
    no_output_noun="modernizations",
    scope_note=(
        "Scan for SOTA-drift modernization opportunities (deprecated/unmaintained deps,"
        " language-feature gaps, format/protocol shifts, major version debt, platform EOL)."
    ),
    prompt_builder=build_modernization_prompt,
)


def run_test_gap(
    store: Store,
    cfg: Config,
    repo: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    """Hunt for test coverage gaps in a repo."""
    return _run_analysis_job(store, cfg, repo, _TEST_GAP_SPEC, backend, resume)


def run_dep_update(
    store: Store,
    cfg: Config,
    repo: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    """Check for outdated dependencies using Renovate's local scanner.

    Zero AI tokens: Renovate handles ecosystem detection, registry queries,
    and semver classification. Results are ingested as dep_update findings
    for triage in the UI; the apply phase (when a user queues one) remains
    AI-powered.

    Falls back to the AI-based analysis job if Renovate fails (not installed,
    timeout, etc.).

    A suspended dep_update can only have come from that AI fallback --
    the Renovate path spawns no worker and so can never be cap-killed --
    so a resume goes straight there rather than re-running the scanner.
    """
    if resume is not None:
        return _run_analysis_job(store, cfg, repo, _DEP_UPDATE_SPEC, backend, resume)

    from .dep_scan import scan_repo  # noqa: PLC0415

    rid = repo["id"]
    rname = repo["name"]
    rpath = Path(repo["path"])

    if not rpath.exists():
        store.log_event("error", f"dep_update {rname}: repo not cloned")
        return {"error": "repo not cloned"}

    # Sync to latest default branch
    for cmd in (
        ["git", "fetch", "origin"],
        ["git", "checkout", repo["default_branch"]],
        ["git", "pull", "--ff-only"],
    ):
        rc, out = run_cmd(["git", "-C", str(rpath), *cmd[1:]], timeout=600)
        if rc != 0:
            store.log_event("error", f"dep_update {rname}: {' '.join(cmd)} failed: {out[-300:]}")
            return {"error": f"{' '.join(cmd)} failed"}

    candidates = scan_repo(rpath, rname)

    if candidates is None:
        # Renovate exited unsuccessfully without output — fall back to AI
        log.info("dep_update %s: renovate failed without output, falling back to AI", rname)
        return _run_analysis_job(store, cfg, repo, _DEP_UPDATE_SPEC, backend)

    # Write candidates to a temp file and ingest via the standard path
    out_path = cfg.work_root / "out" / f"dep_scan_{rid}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(candidates, indent=2))

    counts = ingest_findings(store, rid, out_path, finding_type="dep_update")
    store.log_event(
        "dep_update",
        f"{rname}: renovate scan"
        f" +{counts['inserted']} new / {counts['duplicates']} dup"
        f" / {counts['invalid']} invalid (0 tok)",
    )

    # Advance the rotation timestamp only after valid ingestion
    if counts["invalid"] == 0:
        store.db.execute(
            "UPDATE repos SET last_dep_update_at = ? WHERE id = ?",
            (now_ms(), rid),
        )
        store.db.commit()

    return {
        "kind": "dep_update",
        "repo": rname,
        "state": "done",
        "tokens_new": 0,
        "ingest": counts,
    }


def run_refactor(
    store: Store,
    cfg: Config,
    repo: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    """Hunt for mechanical refactoring opportunities."""
    return _run_analysis_job(store, cfg, repo, _REFACTOR_SPEC, backend, resume)


def run_modernize(
    store: Store,
    cfg: Config,
    repo: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    """Hunt for SOTA-drift modernization opportunities -- deprecated or
    unmaintained dependencies, language-feature gaps, format/protocol
    shifts, major version debt, platform EOL. Explicitly NOT bounded to
    safe/mechanical changes like refactor/dep_update; see modernization.md."""
    return _run_analysis_job(store, cfg, repo, _MODERNIZATION_SPEC, backend, resume)


# -- fix --------------------------------------------------------------------


# Consecutive attempts (of run_fix/run_recheck/run_harvest, tracked
# per-finding) that hit the IDENTICAL failure reason before giving up
# instead of retrying again. A different reason each time still retries
# immediately -- only a stuck, unchanging dead end (same push/PR/commit
# failure, same unparseable-verdict cause, same PR-view error every
# attempt) burns a bounded number of retries rather than looping forever
# at token-window pace (see _compute_sleep_s's 0s-when-queued policy in
# server.py, and pick_next's fixed engage > harvest > recheck > fix
# priority ladder, which a permanently-stuck top-priority item would
# otherwise head-of-line-block forever).
MAX_CONSECUTIVE_SAME_FAILURE = 3


def run_fix(
    store: Store,
    cfg: Config,
    finding: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    fid: int = finding["id"]
    if finding["status"] != "queued":
        return {
            "skipped": (f"finding #{fid} is {finding['status']!r}, not queued"),
        }
    repo = store.get_repo(finding["repo_id"])
    if repo is None:
        store.log_event(
            "error",
            f"fix #{fid}: repo {finding['repo_id']} missing",
            finding_id=fid,
        )
        return {"error": "repo missing"}
    rpath: str = repo["path"]
    db: str = repo["default_branch"]

    finding_type = finding.get("type", "bug")
    is_bug = finding_type == "bug"
    is_modernization = finding_type == "modernization"
    slug = re.sub(r"[^a-zA-Z0-9]+", "-", finding["summary"]).lower().strip("-")[:40].rstrip("-")
    branch_prefix = "fix" if is_bug else ("modernize" if is_modernization else "improve")
    branch = f"{branch_prefix}/{slug}-{fid}"
    worktree = cfg.work_root / "wt" / f"f{fid}"
    worktree.parent.mkdir(parents=True, exist_ok=True)

    # Retry after a kill/failure: reclaim the salvage worktree and branch --
    # committed proof/fix steps live on the branch, but a fresh worker starts
    # from a clean base (its playbook re-verifies the bug anyway).
    #
    # Neither the reclaim nor the add happens on a resume: this is the
    # exact tree the session being continued believes it is sitting in,
    # and rebuilding it from origin would delete the worker's uncommitted
    # work and leave it reading files it has already reasoned about.
    # pick_next only offers a suspension whose worktree is still there.
    if resume is None:
        if worktree.exists():
            run_cmd(
                [
                    "git",
                    "-C",
                    rpath,
                    "worktree",
                    "remove",
                    "--force",
                    str(worktree),
                ]
            )
            run_cmd(["git", "-C", rpath, "branch", "-D", branch])
            store.log_event(
                "fix",
                f"#{fid}: reclaimed stale worktree from prior attempt",
                finding_id=fid,
            )

        rc, out = run_cmd(
            [
                "git",
                "-C",
                rpath,
                "worktree",
                "add",
                "-b",
                branch,
                str(worktree),
                f"origin/{db}",
            ]
        )
        if rc != 0:
            rc, out = run_cmd(
                [
                    "git",
                    "-C",
                    rpath,
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    str(worktree),
                    db,
                ]
            )
        if rc != 0:
            store.log_event(
                "error",
                f"fix #{fid}: worktree add failed: {out[-300:]}",
                finding_id=fid,
            )
            return {"error": f"worktree add failed: {out[-300:]}"}

    def _drop_worktree(delete_branch: bool) -> None:
        run_cmd(
            [
                "git",
                "-C",
                rpath,
                "worktree",
                "remove",
                "--force",
                str(worktree),
            ]
        )
        if delete_branch:
            run_cmd(["git", "-C", rpath, "branch", "-D", branch])

    override = finding.get("budget_override")
    anticipated = (
        resume.anticipated
        if resume is not None
        else anticipated_tokens(store, cfg, repo["id"], "fix")
    )
    outlook = backend.decide(anticipated_tokens=anticipated)
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            # A resume that cannot be funded right now keeps its worktree:
            # dropping it would destroy the suspended session's tree over a
            # decision that is purely about timing, and the next cycle
            # would find nothing to continue.
            if resume is None:
                _drop_worktree(delete_branch=True)
            store.log_event("deny", f"fix #{fid}: {reason}", finding_id=fid)
            return {"denied": reason, "retry_at": retry_at}
        case Granted(cap_tokens=cap):
            pass
    try:
        job = store.create_job(
            "fix",
            repo["id"],
            finding_id=fid,
            cap_tokens=cap,
            state="running",
            estimated_tokens=anticipated,
            resumed_from=resume.predecessor_id if resume is not None else None,
        )
    except ValueError as e:
        # The repo was deleted after this run was picked: skip, don't crash.
        if resume is None:
            _drop_worktree(delete_branch=True)
        return {"skipped": str(e)}
    with store.in_progress(fid, "fixing", fallback="queued"):
        build_prompt = (
            build_fix_prompt
            if is_bug
            else build_apply_modernization_prompt
            if is_modernization
            else build_apply_improvement_prompt
        )
        prompt = (
            RESUME_PROMPT
            if resume is not None
            else build_prompt(finding, worktree, branch, repo, store.repo_notes(repo["id"]))
        )
        model = cfg.model_for("fix")
        rr = backend.run(
            worktree,
            prompt,
            cap_tokens=cap,
            max_wall_s=cfg.fix_max_wall_s,
            job_class=JobClass.FIX,
            resume_from=resume.session_file if resume is not None else None,
        )
        state = _record_job(store, job, rr, model=model)
        summary: Row = {
            "kind": "fix",
            "finding": fid,
            "job": job,
            "state": state,
            "branch": branch,
            "tokens_new": rr.tokens_new,
        }
        if resume is not None:
            summary["resumed_from"] = resume.predecessor_id

        # (a) Worker declined this finding, or hit a blocker.
        decline_file = worktree / ("NOT-A-BUG.md" if is_bug else "DECLINED.md")
        blocked_file = worktree / "BLOCKED.md"
        outcome_file = (
            decline_file
            if decline_file.exists()
            else (blocked_file if blocked_file.exists() else None)
        )
        if outcome_file is not None:
            reason = outcome_file.read_text()[:500]
            store.set_status(fid, "rejected", verdict_reason=reason)
            store.clear_fix_attempts(fid)
            first_line = reason.splitlines()[0][:120] if reason else ""
            verb = "rejected" if outcome_file == decline_file else "blocked"
            store.log_event(
                "fix",
                f"#{fid} {verb} by worker: {first_line}",
                job_id=job,
                finding_id=fid,
            )
            _drop_worktree(delete_branch=True)
            summary["outcome"] = "rejected"
            return summary

        # (b) Commits + PR description -> ship a draft PR.
        rc, commits = run_cmd(
            [
                "git",
                "-C",
                str(worktree),
                "log",
                f"origin/{db}..HEAD",
                "--oneline",
            ]
        )
        if rc != 0:
            rc, commits = run_cmd(
                [
                    "git",
                    "-C",
                    str(worktree),
                    "log",
                    f"{db}..HEAD",
                    "--oneline",
                ]
            )
        pr_desc = worktree / "PR-DESCRIPTION.md"
        failure: str | None = None
        if rc == 0 and commits and pr_desc.exists():
            forge = forge_for(repo)
            push_url = forge.ssh_url(repo["url"])
            rc, out = run_cmd(
                ["git", "-C", str(worktree), "push", "--force", push_url, "HEAD"],
                timeout=600,
            )
            if rc == 0:
                _, title = run_cmd(
                    [
                        "git",
                        "-C",
                        str(worktree),
                        "log",
                        "-1",
                        "--format=%s",
                    ]
                )
                owner_slug = forge.owner_repo(repo["url"])
                if not owner_slug:
                    failure = f"unparseable repo url for PR: {repo['url']!r}"
                else:
                    rc, pr_url_or_err = forge.create_pr(
                        owner_slug,
                        branch,
                        title or branch,
                        pr_desc,
                        cwd=str(worktree),
                    )
                    if rc != 0 and "already exists" in pr_url_or_err:
                        # Prior attempt already created the PR — extract its URL.
                        m = re.search(r"https://\S+/pull/\d+", pr_url_or_err)
                        if m:
                            rc, pr_url_or_err = 0, m.group()
                    if rc == 0:
                        store.set_status(fid, "pr_open", pr_url=pr_url_or_err)
                        store.clear_fix_attempts(fid)
                        store.log_event(
                            "ship",
                            f"#{fid} draft PR: {pr_url_or_err}",
                            job_id=job,
                            finding_id=fid,
                        )
                        _drop_worktree(delete_branch=False)
                        summary.update(outcome="pr_open", pr_url=pr_url_or_err)
                        if override == "once":
                            store.set_budget_override(fid, None)
                        return summary
                    failure = f"PR create failed: {pr_url_or_err[-300:]}"
            else:
                failure = f"push failed: {out[-300:]}"
        elif failure is None:
            failure = (
                ("no commits" if not commits else "no PR-DESCRIPTION.md")
                if state == "done"
                else f"worker {state}"
            )

        # (c) Salvage: either requeue for another attempt, or -- if this
        # exact failure has now recurred MAX_CONSECUTIVE_SAME_FAILURE
        # times in a row with nothing changed in between -- give up and
        # surface it for human attention instead of burning tokens on a
        # retry certain to repeat the identical outcome.
        assert failure is not None  # noqa: S101 -- every branch above sets it
        streak = store.record_fix_attempt(fid, failure)
        tail = (rr.stdout_tail or "")[-300:]
        if streak >= MAX_CONSECUTIVE_SAME_FAILURE:
            store.set_status(
                fid,
                "rejected",
                verdict_reason=(
                    f"stuck: {streak} consecutive fix attempts hit the same failure: {failure}"
                ),
            )
            store.clear_fix_attempts(fid)
            store.log_event(
                "fix",
                f"#{fid} gave up after {streak} identical failures ({failure}); "
                f"worktree kept at {worktree}. tail: {tail}",
                job_id=job,
                finding_id=fid,
            )
            _drop_worktree(delete_branch=True)
            summary.update(outcome="stuck", failure=failure, attempts=streak)
            if override == "once":
                store.set_budget_override(fid, None)
            return summary
        store.set_status(fid, "queued")
        store.log_event(
            "fix",
            f"#{fid} incomplete ({failure}); worktree kept at {worktree}. tail: {tail}",
            job_id=job,
            finding_id=fid,
        )
        summary.update(outcome="requeued", failure=failure, worktree=str(worktree))
        if override == "once":
            store.set_budget_override(fid, None)
        return summary


# -- pr sync ----------------------------------------------------------------

_FAIL_CONCLUSIONS = ("FAILURE", "TIMED_OUT", "CANCELLED")
_REJECT_CLOSED = "PR closed without merge -- treat this bug class/location as human-rejected"


def _iso_ms(ts: str | None) -> int:
    """ISO-8601 timestamp -> epoch ms (0 when absent/unparseable)."""
    if not ts:
        return 0
    try:
        dt = datetime.fromisoformat(ts)
        return int(dt.timestamp() * 1000)
    except ValueError:
        return 0


def _latest_activity_ms(pr: Row) -> int:
    stamps = [_iso_ms(c.get("createdAt")) for c in pr.get("comments") or []]
    stamps += [_iso_ms(r.get("submittedAt")) for r in pr.get("reviews") or []]
    return max(stamps, default=0)


def _checks_summary(
    rollup: list[Row] | None,
) -> tuple[str | None, bool, list[str]]:
    """(short human summary, any_failing, sorted failing check names)."""
    if not rollup:
        return None, False, []
    named = [
        (
            c.get("name") or c.get("context") or "?",
            (c.get("conclusion") or c.get("state") or "").upper(),
        )
        for c in rollup
    ]
    failing = [name for name, concl in named if concl in _FAIL_CONCLUSIONS]
    failing_names = sorted(set(failing))
    passing = sum(1 for _, concl in named if concl in ("SUCCESS", "NEUTRAL", "SKIPPED"))
    pending = len(named) - len(failing) - passing
    parts = [f"{passing} pass"]
    if failing_names:
        parts.append(f"{len(failing)} fail")
    if pending:
        parts.append(f"{pending} pending")
    return " / ".join(parts), bool(failing_names), failing_names


def _attention_fingerprint(pr: Row, failing_names: list[str]) -> str | None:
    """Signature of the STATIC (non-comment) part of "what's wrong" --
    review decision, merge conflicts, and WHICH checks are failing (not
    just whether any are). None means nothing static is wrong.

    Deliberately excludes comment/review activity: that has its own,
    already-correct mechanism (the last_engaged_activity_at watermark --
    a genuinely new comment always produces last_activity > engaged,
    every time, no fingerprint needed). This is only for the reasons
    that can recur identically forever without new information -- the
    same check staying red, the same conflict staying unresolved, the
    same review staying unaddressed -- where "still true" and "true
    again" are indistinguishable from a boolean, but distinguishable
    once you name exactly which checks are failing."""
    review = (pr.get("reviewDecision") or "").upper()
    mergeable = (pr.get("mergeable") or "").upper()
    parts = []
    if review == "CHANGES_REQUESTED":
        parts.append(f"review:{review}")
    if mergeable == "CONFLICTING":
        parts.append(f"mergeable:{mergeable}")
    if failing_names:
        parts.append(f"checks:{','.join(failing_names)}")
    return "|".join(parts) or None


def sync_prs(store: Store, cfg: Config) -> Row:  # noqa: ARG001
    """Refresh pr_state for every pr_open finding.

    Forge reads only -- no tokens, never raises; a forge CLI failure logs
    an event and skips that PR.
    """
    summary: Row = {
        "synced": 0,
        "merged": 0,
        "closed": 0,
        "attention": 0,
        "errors": 0,
    }
    for f in store.list_findings(status="pr_open"):
        fid: int = f["id"]
        url: str = f.get("pr_url") or ""
        repo = store.get_repo(f["repo_id"])
        if repo is None:
            store.log_event(
                "error",
                f"sync #{fid}: repo {f['repo_id']} missing",
                finding_id=fid,
            )
            summary["errors"] += 1
            continue
        forge = forge_for(repo)
        parsed = forge.parse_pr_url(url)
        if not parsed:
            store.log_event(
                "error",
                f"sync #{fid}: unparseable pr_url {url!r}",
                finding_id=fid,
            )
            summary["errors"] += 1
            continue
        slug, num = parsed
        rc, pr, raw = forge.view_pr_sync(slug, num)
        if rc != 0 or pr is None:
            store.log_event(
                "error",
                f"sync #{fid}: PR/MR view failed: {(raw or '')[-300:]}",
                finding_id=fid,
            )
            summary["errors"] += 1
            continue

        pr_state = (pr.get("state") or "").upper()
        if pr_state == "MERGED":
            store.set_status(fid, "merged")
            store.upsert_pr_state(
                fid,
                pr_number=num,
                state=pr_state,
                needs_attention=None,
                synced_at=now_ms(),
            )
            store.log_event("ship", f"#{fid} PR merged: {url}", finding_id=fid)
            summary["merged"] += 1
            continue
        if pr_state == "CLOSED":
            store.set_status(fid, "rejected", verdict_reason=_REJECT_CLOSED)
            store.upsert_pr_state(
                fid,
                pr_number=num,
                state=pr_state,
                needs_attention=None,
                synced_at=now_ms(),
            )
            store.log_event(
                "verdict",
                f"#{fid} PR closed without merge: {url}",
                finding_id=fid,
            )
            summary["closed"] += 1
            continue

        prev = store.get_pr_state(fid)
        last_activity = _latest_activity_ms(pr)
        checks, failing, failing_names = _checks_summary(pr.get("statusCheckRollup"))
        if prev is None or prev.get("last_engaged_activity_at") is None:
            # First sync: the PR-creation chatter is our own -- baseline the
            # watermark at the PR's current activity without flagging.
            engaged = max(_iso_ms(pr.get("updatedAt")), last_activity)
        else:
            engaged = prev["last_engaged_activity_at"]

        # Suppression: don't re-flag a static (non-comment) reason that
        # is IDENTICAL to the one an engage cycle already declined to
        # fix without any new information -- see run_engage's
        # attention_fingerprint comment for the production incident this
        # replaced a time-based backoff for. new_comments is exempt: a
        # genuinely new comment always deserves a fresh look regardless
        # of whether the static situation is unchanged.
        fp = _attention_fingerprint(pr, failing_names)
        addressed_fp = prev.get("addressed_fingerprint") if prev else None
        addressed_sha = prev.get("addressed_head_sha") if prev else None
        head_sha = pr.get("headRefOid")
        # A push that happens to leave the SAME check name red (or the
        # same review/conflict state) is still new information -- the
        # code changed even if today's static snapshot looks identical to
        # what was declined before. Require the head sha to match too, not
        # just the fingerprint string, before trusting the suppression.
        suppressed = (
            fp is not None
            and fp == addressed_fp
            and addressed_sha is not None
            and head_sha == addressed_sha
        )

        reasons: list[str] = []
        if last_activity > (engaged or 0):
            reasons.append("new_comments")
        if not suppressed:
            if (pr.get("reviewDecision") or "").upper() == "CHANGES_REQUESTED":
                reasons.append("changes_requested")
            if (pr.get("mergeable") or "").upper() == "CONFLICTING":
                reasons.append("conflict")
            if failing:
                reasons.append("checks_failing")
        attention = ",".join(reasons) or None

        # attention_since is the fairness fix (see list_attention's
        # docstring) -- only touch it when the reason actually CHANGED.
        # Left alone otherwise, so it keeps reflecting when THIS reason
        # first appeared, not "whenever sync_prs last ran" (every cycle,
        # for every pr_open finding).
        prev_attention = prev.get("needs_attention") if prev else None
        reason_fields: dict[str, int | None] = {}
        if attention != prev_attention:
            reason_fields["attention_since"] = now_ms() if attention else None
        # Any change from what was last addressed (resolved to nothing
        # wrong, changed to a genuinely different problem, OR the code
        # moved since the decline) clears the stale marker -- so a LATER
        # recurrence of the ORIGINAL problem, even after an unrelated one
        # intervened in between, is treated as fresh rather than
        # auto-suppressed by memory of a since-superseded decline.
        # Comparing here, before the write below, against the fingerprint
        # and sha this decision (suppressed, above) was actually based on.
        clear_addressed = (
            {"addressed_fingerprint": None, "addressed_head_sha": None}
            if addressed_fp and (fp != addressed_fp or head_sha != addressed_sha)
            else {}
        )

        store.upsert_pr_state(
            fid,
            pr_number=num,
            state=pr_state,
            mergeable=pr.get("mergeable"),
            checks=checks,
            head_ref=pr.get("headRefName"),
            head_sha=head_sha,
            last_activity_at=last_activity,
            last_engaged_activity_at=engaged,
            needs_attention=attention,
            attention_fingerprint=fp,
            synced_at=now_ms(),
            **reason_fields,
            **clear_addressed,
        )
        if attention and (prev is None or prev.get("needs_attention") != attention):
            store.log_event(
                "engage",
                f"#{fid} PR #{num} needs attention: {attention}",
                finding_id=fid,
            )
        summary["synced"] += 1
        if attention:
            summary["attention"] += 1
    return summary


# -- engage -----------------------------------------------------------------


def run_engage(
    store: Store,
    cfg: Config,
    finding: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    fid: int = finding["id"]
    repo = store.get_repo(finding["repo_id"])
    if repo is None:
        store.log_event(
            "error",
            f"engage #{fid}: repo {finding['repo_id']} missing",
            finding_id=fid,
        )
        return {"error": "repo missing"}
    ps = store.get_pr_state(fid)
    if not ps or not ps.get("pr_number") or not ps.get("head_ref"):
        store.log_event(
            "error",
            f"engage #{fid}: no pr_state/head_ref -- sync first",
            finding_id=fid,
        )
        return {"error": "no pr_state"}
    forge = forge_for(repo)
    owner_slug = forge.owner_repo(repo["url"])
    if not owner_slug:
        store.log_event(
            "error",
            f"engage #{fid}: unparseable repo url {repo['url']!r}",
            finding_id=fid,
        )
        return {"error": "unparseable repo url"}
    rpath: str = repo["path"]
    num: int = ps["pr_number"]
    head_ref: str = ps["head_ref"]
    if head_ref == repo["default_branch"]:
        # Structurally shouldn't happen (a PR's head can't be its own base
        # in the same repo) but costs nothing to refuse outright: engage
        # pushes are allowed to rewrite the PR branch freely (that's the
        # point -- squash/cleanup before merge), the one thing that must
        # never happen is a rewriting push landing on the default branch.
        store.log_event(
            "error",
            f"engage #{fid}: refusing -- head_ref equals default branch {head_ref!r}",
            finding_id=fid,
        )
        return {"error": "head_ref equals default branch"}

    worktree = cfg.work_root / "wt" / f"e{fid}"
    worktree.parent.mkdir(parents=True, exist_ok=True)
    # A resume touches none of this: the worktree below is the tree the
    # session being continued believes it is in, so reclaiming and
    # re-adding it would delete the worker's work in progress. pick_next
    # only offers a suspension whose worktree is still present.
    if resume is None:
        if worktree.exists():
            run_cmd(
                [
                    "git",
                    "-C",
                    rpath,
                    "worktree",
                    "remove",
                    "--force",
                    str(worktree),
                ]
            )
            store.log_event(
                "engage",
                f"#{fid}: reclaimed stale worktree from prior attempt",
                finding_id=fid,
            )

        rc, out = run_cmd(["git", "-C", rpath, "fetch", "origin", head_ref], timeout=600)
        if rc != 0:
            store.log_event(
                "error",
                f"engage #{fid}: fetch {head_ref} failed: {out[-300:]}",
                finding_id=fid,
            )
            return {"error": f"fetch failed: {out[-300:]}"}
        rc, out = run_cmd(
            [
                "git",
                "-C",
                rpath,
                "worktree",
                "add",
                "--detach",
                str(worktree),
                f"origin/{head_ref}",
            ]
        )
        if rc != 0:
            store.log_event(
                "error",
                f"engage #{fid}: worktree add failed: {out[-300:]}",
                finding_id=fid,
            )
            return {"error": f"worktree add failed: {out[-300:]}"}
        # Best effort: put the branch itself on HEAD (push works detached too).
        run_cmd(
            [
                "git",
                "-C",
                str(worktree),
                "checkout",
                "-B",
                head_ref,
                f"origin/{head_ref}",
            ]
        )

    def _drop_worktree() -> None:
        run_cmd(
            [
                "git",
                "-C",
                rpath,
                "worktree",
                "remove",
                "--force",
                str(worktree),
            ]
        )

    override = finding.get("budget_override")
    anticipated = (
        resume.anticipated
        if resume is not None
        else anticipated_tokens(store, cfg, repo["id"], "engage")
    )
    outlook = backend.decide(anticipated_tokens=anticipated)
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            # Keep a resumed attempt's worktree: it holds a live session's
            # work, and a denial is about timing, not about that work.
            if resume is None:
                _drop_worktree()
            store.log_event("deny", f"engage #{fid}: {reason}", finding_id=fid)
            return {"denied": reason, "retry_at": retry_at}
        case Granted(cap_tokens=cap):
            pass

    rc, pr, raw = forge.view_pr_engage(owner_slug, num)
    if pr is None:
        store.log_event(
            "error",
            f"engage #{fid}: PR/MR view failed: {(raw or '')[-300:]}",
            finding_id=fid,
        )
        if resume is None:
            _drop_worktree()
        return {"error": "PR/MR view failed"}

    try:
        job = store.create_job(
            "engage",
            repo["id"],
            finding_id=fid,
            cap_tokens=cap,
            state="running",
            estimated_tokens=anticipated,
            resumed_from=resume.predecessor_id if resume is not None else None,
        )
    except ValueError as e:
        # The repo was deleted after this run was picked: skip, don't crash.
        if resume is None:
            _drop_worktree()
        return {"skipped": str(e)}
    prompt = (
        RESUME_PROMPT
        if resume is not None
        else build_engage_prompt(
            worktree,
            head_ref,
            repo,
            pr,
            ps.get("needs_attention") or "",
            store.repo_notes(repo["id"]),
        )
    )
    model = cfg.model_for("fix")
    rr = backend.run(
        worktree,
        prompt,
        cap_tokens=cap,
        max_wall_s=cfg.fix_max_wall_s,
        job_class=JobClass.FIX,
        resume_from=resume.session_file if resume is not None else None,
    )
    state = _record_job(store, job, rr, model=model)
    summary: Row = {
        "kind": "engage",
        "finding": fid,
        "job": job,
        "state": state,
        "pr": num,
        "tokens_new": rr.tokens_new,
    }
    if resume is not None:
        summary["resumed_from"] = resume.predecessor_id

    # (a) Worker concluded the fix should be abandoned.
    withdraw = worktree / "WITHDRAW.md"
    if withdraw.exists():
        reason = withdraw.read_text()
        forge.close_pr(owner_slug, num, reason[:800])
        store.set_status(fid, "rejected", verdict_reason=reason[:500])
        store.upsert_pr_state(
            fid,
            state="CLOSED",
            needs_attention=None,
            synced_at=now_ms(),
        )
        first_line = reason.splitlines()[0][:120] if reason else ""
        store.log_event(
            "verdict",
            f"#{fid} withdrawn by engage worker: {first_line}",
            job_id=job,
            finding_id=fid,
        )
        # A withdrawal often means something else moved first (e.g. a
        # sibling dep bump landed and changed what's achievable) -- worth
        # distinguishing "fully done, nothing left" from "the goal is now
        # MORE reachable, not less" (see engage.md's superseded-PR
        # guidance). Read before _drop_worktree below removes the file.
        _ingest_followups(store, repo["id"], worktree, fid, job, "engage")
        _drop_worktree()
        summary["outcome"] = "withdrawn"
        return summary

    # (b) Push new commits, post the reply comment.
    failure: str | None = None if state == "done" else f"worker {state}"
    pushed = replied = False
    if failure is None:
        rc, commits = run_cmd(
            [
                "git",
                "-C",
                str(worktree),
                "log",
                f"origin/{head_ref}..HEAD",
                "--oneline",
            ]
        )
        if rc == 0 and commits:
            rc, out = run_cmd(
                [
                    "git",
                    "-C",
                    str(worktree),
                    "push",
                    "--force",
                    forge.ssh_url(repo["url"]),
                    f"HEAD:{head_ref}",
                ],
                timeout=600,
            )
            if rc == 0:
                pushed = True
            else:
                failure = f"push failed: {out[-300:]}"
    if failure is None:
        reply = worktree / "PR-REPLY.md"
        if reply.exists():
            rc, out = forge.comment_pr(owner_slug, num, reply)
            if rc == 0:
                replied = True
            else:
                failure = f"PR comment failed: {out[-300:]}"

    # (c) Failure: keep the worktree and the attention flag -- retry next
    # cycle.
    if failure is not None:
        if state == "done":
            store.update_job(job, state="failed", notes=failure)
        tail = (rr.stdout_tail or "")[-300:]
        store.log_event(
            "engage",
            f"#{fid} incomplete ({failure}); worktree kept at {worktree}. tail: {tail}",
            job_id=job,
            finding_id=fid,
        )
        summary.update(outcome="retry", failure=failure, worktree=str(worktree))
        if override == "once":
            store.set_budget_override(fid, None)
        return summary

    # Watermark: activity up to the sync snapshot is handled; when we just
    # posted our own comment, advance to now + 3s to absorb clock skew
    # between local time and GitHub's createdAt timestamp.
    engaged_mark = (now_ms() + 3_000) if replied else (ps.get("last_activity_at") or now_ms())
    # Loop-breaker, state-based not time-based (see sync_prs's
    # addressed_fingerprint comparison for the full mechanism): if
    # nothing was pushed, record the static-state fingerprint we just
    # declined to fix (ps["attention_fingerprint"] -- computed by
    # sync_prs earlier THIS SAME cycle, since it always runs immediately
    # before pick_next/run_engage; see run_cycle). sync_prs then
    # suppresses re-flagging the identical static reason for as long as
    # the underlying facts (which checks fail, review state, conflict)
    # stay unchanged -- no matter how many cycles or how much wall-clock
    # time that takes, unlike an earlier version of this fix that used a
    # flat time-based backoff (rejected: an unattended daemon running for
    # months would just keep re-poking an already-explained, genuinely
    # unfixable problem every N minutes forever). A push clears it
    # instead: a real attempt was made, worth a genuinely fresh look.
    #
    # Deliberately NOT clearing needs_attention here (unlike the withdraw
    # branch above, where it's moot -- status leaves pr_open entirely):
    # sync_prs always runs before pick_next on every cycle and
    # unconditionally recomputes needs_attention from live GitHub state,
    # so clearing it here was always redundant for correctness -- and
    # was actively WRONG in an earlier version of this fix, once
    # attention_since existed: it made sync_prs's very next pass see a
    # false None -> reason transition and wipe state this same call had
    # just set (caught live before shipping).
    store.upsert_pr_state(
        fid,
        last_engaged_activity_at=engaged_mark,
        synced_at=now_ms(),
        addressed_fingerprint=None if pushed else ps.get("attention_fingerprint"),
        addressed_head_sha=None if pushed else ps.get("head_sha"),
    )
    did = [b for b, on in (("pushed", pushed), ("replied", replied)) if on] or ["no-op"]
    store.log_event(
        "engage",
        f"#{fid} PR #{num} engaged ({', '.join(did)})",
        job_id=job,
        finding_id=fid,
    )
    _drop_worktree()
    summary.update(outcome="engaged", pushed=pushed, replied=replied)
    if override == "once":
        store.set_budget_override(fid, None)
    return summary


# -- harvest ------------------------------------------------------------


def run_harvest(
    store: Store,
    cfg: Config,
    finding: Row,
    backend: Backend,
    *,
    resume: ResumePlan | None = None,
) -> Row:
    """Review a just-merged PR's complete lifetime (title, body, every
    comment/review, and the actual shipped diff) to propose genuine
    follow-up findings.

    Deliberately a separate pass from run_fix's ship-time ingestion (now
    removed) and run_engage's withdraw-time one: a PR's true final scope
    is only known once it's done. What looked deferred when the PR opened
    can collapse entirely (a human pushes further through ordinary
    review, closing the gap inside the SAME PR -- observed live: a
    dep_update PR's target grew from an intermediate version to its full
    original goal purely through engage replies, with the PR body's own
    "what was deliberately NOT changed" text never updated to match) and
    what wasn't yet known can newly emerge (a reviewer flags a cosmetic
    deprecation warning as "separate refactor work" in a late comment --
    also observed on the same PR). A point-in-time snapshot taken during
    the PR's open life structurally cannot see either of these.
    """
    fid: int = finding["id"]
    repo = store.get_repo(finding["repo_id"])
    if repo is None:
        store.log_event(
            "error",
            f"harvest #{fid}: repo {finding['repo_id']} missing",
            finding_id=fid,
        )
        return {"error": "repo missing"}
    ps = store.get_pr_state(fid)
    if not ps or not ps.get("pr_number"):
        store.log_event(
            "error",
            f"harvest #{fid}: no pr_state/pr_number -- sync first",
            finding_id=fid,
        )
        return {"error": "no pr_state"}
    forge = forge_for(repo)
    owner_slug = forge.owner_repo(repo["url"])
    if not owner_slug:
        store.log_event(
            "error",
            f"harvest #{fid}: unparseable repo url {repo['url']!r}",
            finding_id=fid,
        )
        return {"error": "unparseable repo url"}
    rpath: str = repo["path"]
    num: int = ps["pr_number"]
    default_branch: str = repo["default_branch"]

    worktree = cfg.work_root / "wt" / f"h{fid}"
    worktree.parent.mkdir(parents=True, exist_ok=True)
    # Left alone on a resume: this is the tree the continued session
    # believes it is in (see run_fix for the same reasoning).
    if worktree.exists() and resume is None:
        run_cmd(["git", "-C", rpath, "worktree", "remove", "--force", str(worktree)])
        store.log_event(
            "harvest",
            f"#{fid}: reclaimed stale worktree from prior attempt",
            finding_id=fid,
        )

    def _drop_worktree() -> None:
        run_cmd(["git", "-C", rpath, "worktree", "remove", "--force", str(worktree)])

    if resume is None:
        rc, out = run_cmd(["git", "-C", rpath, "fetch", "origin", default_branch], timeout=600)
        if rc != 0:
            store.log_event(
                "error",
                f"harvest #{fid}: fetch {default_branch} failed: {out[-300:]}",
                finding_id=fid,
            )
            return {"error": f"fetch failed: {out[-300:]}"}
        rc, out = run_cmd(
            [
                "git",
                "-C",
                rpath,
                "worktree",
                "add",
                "--detach",
                str(worktree),
                f"origin/{default_branch}",
            ]
        )
        if rc != 0:
            store.log_event(
                "error",
                f"harvest #{fid}: worktree add failed: {out[-300:]}",
                finding_id=fid,
            )
            return {"error": f"worktree add failed: {out[-300:]}"}

    override = finding.get("budget_override")
    anticipated = (
        resume.anticipated
        if resume is not None
        else anticipated_tokens(store, cfg, repo["id"], "harvest")
    )
    outlook = backend.decide(anticipated_tokens=anticipated)
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            # Keep a resumed attempt's worktree: a denial is about timing,
            # not about the live session's work sitting in it.
            if resume is None:
                _drop_worktree()
            store.log_event("deny", f"harvest #{fid}: {reason}", finding_id=fid)
            return {"denied": reason, "retry_at": retry_at}
        case Granted(cap_tokens=cap):
            pass

    rc, pr, raw = forge.view_pr_engage(owner_slug, num)
    if pr is None:
        store.log_event(
            "error",
            f"harvest #{fid}: PR/MR view failed: {(raw or '')[-300:]}",
            finding_id=fid,
        )
        if resume is None:
            _drop_worktree()
        return {"error": "PR/MR view failed"}

    try:
        job = store.create_job(
            "harvest",
            repo["id"],
            finding_id=fid,
            cap_tokens=cap,
            state="running",
            estimated_tokens=anticipated,
            resumed_from=resume.predecessor_id if resume is not None else None,
        )
    except ValueError as e:
        # The repo was deleted after this run was picked: skip, don't crash.
        if resume is None:
            _drop_worktree()
        return {"skipped": str(e)}
    prompt = (
        RESUME_PROMPT
        if resume is not None
        else build_harvest_prompt(finding, worktree, repo, pr, num, store.repo_notes(repo["id"]))
    )
    model = cfg.model_for("fix")
    rr = backend.run(
        worktree,
        prompt,
        cap_tokens=cap,
        max_wall_s=cfg.fix_max_wall_s,
        job_class=JobClass.FIX,
        resume_from=resume.session_file if resume is not None else None,
    )
    state = _record_job(store, job, rr, model=model)

    # Read before _drop_worktree below removes the file.
    _ingest_followups(store, repo["id"], worktree, fid, job, "harvest")
    _drop_worktree()

    summary: Row = {"kind": "harvest", "finding": fid, "job": job, "state": state, "pr": num}
    if resume is not None:
        summary["resumed_from"] = resume.predecessor_id
    if state != "done":
        failure = f"worker {state}"
        streak = store.record_harvest_attempt(fid, failure)
        if streak >= MAX_CONSECUTIVE_SAME_FAILURE:
            # Same harvest failure N times running (e.g. a worker that
            # always gets cap-killed on this PR's diff): stop retrying at
            # top scheduling priority forever (see pick_next's engage >
            # harvest > recheck > fix ladder) and mark it done-trying.
            # harvested_at's meaning broadens slightly to "no further
            # harvest cycles needed" rather than strictly "succeeded";
            # the event log is the audit trail distinguishing the two.
            store.upsert_pr_state(fid, harvested_at=now_ms())
            store.clear_harvest_attempts(fid)
            store.log_event(
                "error",
                f"harvest #{fid}: gave up after {streak} identical failures"
                f" ({failure}) -- not reviewed, will not retry",
                job_id=job,
                finding_id=fid,
            )
            summary["outcome"] = "stuck"
            if override == "once":
                store.set_budget_override(fid, None)
            return summary
        # Leave harvested_at unset so this finding is reconsidered next
        # cycle -- matches run_engage's own failure-retry pattern.
        store.log_event(
            "error",
            f"harvest #{fid}: worker {state}, will retry",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "retry"
        if override == "once":
            store.set_budget_override(fid, None)
        return summary

    store.upsert_pr_state(fid, harvested_at=now_ms())
    store.clear_harvest_attempts(fid)
    store.log_event(
        "harvest",
        f"#{fid} PR #{num} reviewed for follow-ups",
        job_id=job,
        finding_id=fid,
    )
    summary["outcome"] = "harvested"
    if override == "once":
        store.set_budget_override(fid, None)
    return summary


# -- cycle ------------------------------------------------------------------


# Deterministic priority among ties in pick_next's never-run job-type set,
# NOT alphabetical sorting's accident (which put dep_update ahead of hunt
# purely because 'd' < 'h' -- a denied hunt attempt leaves last_hunt_at at
# 0, so this set is exactly what a denial produces. Without an explicit
# order, a freshly-denied hunt lost every tie to dep_update instead of
# being retried -- observed in production: repo `recentIP`'s hunt was
# denied for budget, and the very next cycle ran dep_update instead of
# retrying hunt, purely because of string sort order). Order matches
# pick_next's own docstring: hunt first (discovers what everything else
# would act on), then the three routine maintenance scans, modernization
# last (it's already the slow, periodic one).
_JOB_TYPE_PRIORITY = ("hunt", "test_gap", "dep_update", "refactor", "modernization")


def _resume_origin(store: Store, job_id: int) -> int:
    """The job whose prompt the resumed worker is still following.

    hunt and the analysis kinds bake `out/job{N}.findings.json` into the
    prompt, and a resumed worker is continuing that same transcript: it
    writes where the ORIGINAL prompt told it to, not where the new row's
    id would put it. Ingesting the successor's path would find nothing,
    and since the watermark only advances once output has been ingested,
    every resumed hunt would leave the watermark exactly where it was --
    rebuilding the loop this feature exists to end (one repo ran 10
    consecutive hunts over the identical diff range for 1.48M tokens, 8
    of them producing nothing, because a killed job advanced no watermark
    and was re-selected from scratch).

    Walking, rather than taking the immediate predecessor: the second
    resume of a chain is still following the FIRST job's prompt.
    Depth-capped for the same reason store.resume_chain_tokens is -- the
    write path cannot produce a cycle (resumed_from is set once at INSERT
    to an id that already exists, so ids strictly decrease along the
    link), but a hand-repaired database must not be able to hang a cycle.
    """
    origin = job_id
    for _ in range(_RESUME_ORIGIN_MAX_DEPTH):
        row = store.db.execute("SELECT resumed_from FROM jobs WHERE id = ?", (origin,)).fetchone()
        if row is None or row["resumed_from"] is None:
            break
        origin = int(row["resumed_from"])
    return origin


def _resume_cwd(cfg: Config, kind: str, repo: Row, finding_id: int | None) -> Path | None:
    """Where a resumed worker for this job would have to run, or None when
    that cannot be derived. Mirrors each executor's own choice: the clone
    for repo-level work and for recheck, the per-finding worktree for the
    three kinds that build one.
    """
    prefix = _WORKTREE_PREFIX.get(kind)
    if prefix is None:
        return Path(repo["path"])
    if finding_id is None:
        return None
    return cfg.work_root / "wt" / f"{prefix}{finding_id}"


def _pick_resume(store: Store, cfg: Config) -> tuple[str, Row, ResumePlan] | None:
    """The suspended attempt this cycle should continue, if any.

    Ranked after everything finding-driven and before repo rotation:
    those earlier tiers are user-visible PR interactions with a person
    waiting on them, while a resume is background work that has ALREADY
    been paid for -- so it outranks STARTING new background work, but
    never a human.

    The give-up ceiling is applied here, which makes this the one place
    selection writes. It has to be: leaving a hopeless chain Suspended
    would re-offer it every cycle, and because this tier outranks
    rotation that is not a cheap no-op but permanent starvation of every
    hunt behind it. The write is best effort -- this function also backs
    the Status page's read-only preview, and the candidate is skipped
    either way, so a failed write only defers the marking one cycle.
    """
    for job in store.list_resumable_jobs():
        kind = str(job["kind"])
        repo_id = int(job["repo_id"])
        repo = store.get_repo(repo_id)
        if repo is None:
            continue
        typical = anticipated_tokens(store, cfg, repo_id, kind)
        chain_spent = store.resume_chain_tokens(int(job["id"]))
        if chain_spent > GIVE_UP_MULTIPLE * typical:
            with contextlib.suppress(Exception):
                store.update_job(int(job["id"]), state="failed", killed_reason="give-up")
                store.log_event(
                    "resume",
                    f"resume {kind} {repo['name']}: giving up after {chain_spent} tok"
                    f" across the chain (> {GIVE_UP_MULTIPLE}x {typical} typical)",
                    job_id=int(job["id"]),
                )
            continue
        # The executor's own target shape: a finding row for the kinds
        # driven by one, the repo row for the rest.
        finding_id = job["finding_id"]
        target: Row | None = repo
        if kind in _FINDING_KINDS:
            target = store.get_finding(int(finding_id)) if finding_id is not None else None
        if target is None:
            continue
        cwd = _resume_cwd(cfg, kind, repo, finding_id)
        if cwd is None or not cwd.exists():
            # Nothing to continue INTO: the transcript describes a tree
            # that is gone. Skipped at selection rather than refused in
            # the executor, because a refusal would leave the row
            # Suspended and re-offered every cycle -- and this tier
            # outranks rotation, so that would starve it permanently.
            continue
        session_file = Path(str(job["session_file"]))
        ctx = ctx_at_suspension(session_file)
        if ctx is None:
            # Unreadable transcript, or one with no usage record yet: fall
            # back to the per-kind estimate. It answers a different
            # question (a whole job's spend, not one call's context) but
            # it is the only other measured number available, and
            # reserving nothing here is what lets a cycle overshoot the
            # ramp.
            ctx = typical
        return (
            kind,
            target,
            ResumePlan(
                predecessor_id=int(job["id"]),
                session_file=session_file,
                origin_job_id=_resume_origin(store, int(job["id"])),
                repo_name=str(repo["name"]),
                # What the suspension was carrying, plus what is left of a
                # typical job for this kind -- floored, because a chain
                # that has already outspent the typical job still needs
                # enough budget to do real work rather than re-cache and
                # die.
                anticipated=ctx + max(typical - chain_spent, MIN_PROGRESS_TOKENS),
                ctx=ctx,
                typical=typical,
                chain_spent=chain_spent,
            ),
        )
    return None


def pick_next(
    store: Store,
    cfg: Config,
    force_repo: str | None = None,
) -> tuple[str, Row, ResumePlan | None] | None:
    """Selection: what run_cycle would act on right now, if invoked -- no
    budget check (each run_* function evaluates its own budget when
    actually executed; this only answers "what", not "would it currently
    be allowed"). This is the single place the priority order is
    expressed; run_cycle and the Status page's "what's next" preview both
    call it, so the preview can never drift from what actually runs.

    Side-effect-free except on one path: a resume chain past the give-up
    ceiling is retired here (see _pick_resume for why it cannot be left
    to the executor). That write is best effort and idempotent -- the row
    leaves 'suspended' -- so the preview calling this remains safe.

    Priority: a budget-overridden finding (any category) jumps the
    queue -> flagged PR (oldest-outstanding reason first) -> oldest merged PR pending
    follow-up review (oldest merge first) -> oldest rechecking -> oldest
    queued fix -> the oldest-paid-for suspended attempt that can still be
    continued (see _pick_resume: already-paid-for background work
    outranks starting new background work, but never a person waiting on
    a PR) -> the most stale-of-rotation job type for the
    least-recently-hunted enabled repo (hunt if never cloned, else
    whichever of hunt/test_gap/dep_update/refactor/modernization is
    oldest/never-run, subject to minimum intervals: each scan type
    is gated by cfg.scan_interval_days (default 1 day) since its last
    run for that repo; modernization uses its own longer
    cfg.modernization_interval_days (default 30 days). If all types
    are gated for one repo, the next-stalest repo is tried).

    Returns (kind, target, resume): target is a finding row for
    engage/harvest/recheck/fix, a repo row for
    hunt/test_gap/dep_update/refactor/modernization. `resume` is the plan
    for continuing a suspended attempt of that same kind, or None for
    work starting cold. None means nothing to do (no
    queued/attention/pending-harvest/rechecking work, nothing resumable,
    and no enabled repos).
    """
    rechecking = store.list_findings(status="rechecking")
    attention = store.list_attention()
    pending_harvest = store.list_pending_harvest()
    queued = store.list_findings(status="queued")

    for kind, items in (
        ("engage", attention),
        ("harvest", pending_harvest),
        ("recheck", rechecking),
        ("fix", queued),
    ):
        for f in items:
            if f.get("budget_override"):
                return kind, f, None

    if attention:
        return "engage", attention[0], None  # oldest-flagged reason first (see list_attention)
    if pending_harvest:
        return "harvest", pending_harvest[0], None  # oldest merge first
    if rechecking:
        return "recheck", rechecking[-1], None  # DESC -> last = oldest
    if queued:
        return "fix", queued[-1], None  # DESC -> last = oldest

    resumable = _pick_resume(store, cfg)
    if resumable is not None:
        return resumable

    repos = [r for r in store.list_repos() if r["enabled"]]
    if force_repo:
        repo = store.get_repo(force_repo)
        if repo is None:
            msg = f"unknown repo {force_repo!r}"
            raise ValueError(msg)
        candidates = [repo]
    else:
        # Try repos in staleness order (least-recently-hunted first)
        candidates = (
            sorted(
                repos,
                key=lambda r: (r["last_hunt_at"] is not None, r["last_hunt_at"] or 0),
            )
            if repos
            else []
        )

    scan_interval_ms = int(cfg.scan_interval_days * 86_400_000)
    mod_interval_ms = cfg.modernization_interval_days * 86_400_000
    now = now_ms()

    for target in candidates:
        rpath = Path(target["path"])
        if not rpath.exists():
            return "hunt", target, None  # not cloned yet -> hunt does the clone

        job_times: dict[str, int] = {}
        for kind, key in (
            ("hunt", "last_hunt_at"),
            ("test_gap", "last_test_gap_at"),
            ("dep_update", "last_dep_update_at"),
            ("refactor", "last_refactor_at"),
        ):
            last = target.get(key) or 0
            if last == 0 or (now - last) >= scan_interval_ms:
                job_times[kind] = last

        last_modernization = target.get("last_modernization_at") or 0
        if last_modernization == 0 or (now - last_modernization) >= mod_interval_ms:
            job_times["modernization"] = last_modernization

        if not job_times:
            continue  # all scan types ran within their intervals for this repo

        never_run = [k for k, v in job_times.items() if v == 0]
        job_type = (
            next(k for k in _JOB_TYPE_PRIORITY if k in never_run)
            if never_run
            else min(job_times, key=lambda k: job_times[k])
        )
        return job_type, target, None

    return None


class _Runner(Protocol):
    """Every executor's shape once resume exists: the four positional
    arguments run_cycle dispatches with, plus the optional plan that turns
    the run into a continuation of a suspended attempt. Positional-only so
    the concrete functions keep their own parameter names (repo/finding).
    """

    def __call__(
        self,
        store: Store,
        cfg: Config,
        target: Row,
        backend: Backend,
        /,
        *,
        resume: ResumePlan | None = None,
    ) -> Row: ...


_RUNNERS: dict[str, _Runner] = {
    "engage": run_engage,
    "harvest": run_harvest,
    "recheck": run_recheck,
    "fix": run_fix,
    "hunt": run_hunt,
    "test_gap": run_test_gap,
    "dep_update": run_dep_update,
    "refactor": run_refactor,
    "modernization": run_modernize,
}


def run_cycle(store: Store, cfg: Config, force_repo: str | None = None, *, backend: Backend) -> Row:
    try:
        # (0) Cheap PR sync — gh reads only, no tokens.
        has_pr_open = bool(store.list_findings(status="pr_open"))
        sync: Row | None = sync_prs(store, cfg) if has_pr_open else None

        picked = pick_next(store, cfg, force_repo)
        if picked is None:
            result: Row = {"idle": "no queued findings, no enabled repos"}
            if sync is not None:
                result["sync"] = sync
            store.log_event("cycle", "idle: nothing to do")
            return result

        kind, target, resume = picked
        if resume is not None:
            # Logged here rather than in pick_next: the Status page calls
            # that function on every poll, and an event per poll would
            # bury the log in previews of work nobody ran.
            store.log_event(
                "resume",
                f"resume {kind} {resume.repo_name}: job {resume.predecessor_id}"
                f" -> reserving {resume.anticipated} tok (ctx {resume.ctx}"
                f" + max({resume.typical} - {resume.chain_spent}, {MIN_PROGRESS_TOKENS}))",
                job_id=resume.predecessor_id,
            )
        result = _RUNNERS[kind](store, cfg, target, backend, resume=resume)

        # Prevent starvation of repo-level rotation jobs: if this attempt
        # didn't succeed (failed, errored, or done without output), still
        # bump the timestamp so this job type stops being the perpetual
        # "oldest" pick. Without this, a job type that succeeded once and
        # then fails persistently keeps a frozen-old timestamp while its
        # siblings' timestamps advance past it on their own successes --
        # min()-based fairness then re-selects the stuck job type every
        # cycle forever, starving hunt/test_gap/dep_update/refactor/
        # modernization of their turns. hunt has its own watermark logic
        # (set_last_hunt) and is exempt.
        #
        # A cap kill no longer arrives here as 'killed': it is 'suspended'
        # and therefore deliberately NOT bumped. Re-selection of that work
        # is no longer this bump's job -- the resume tier claims it by id
        # at a priority above rotation entirely. If the chain later proves
        # hopeless the give-up ceiling retires it, and the still-old
        # timestamp is then the honest record that this scan has not run.
        if kind in ("test_gap", "dep_update", "refactor", "modernization"):
            failed = (
                result.get("state") in ("killed", "failed")
                or result.get("error")
                or result.get("ingest_error")
                or (result.get("ingest") or {}).get("invalid", 0) > 0
                or ("ingest" not in result and result.get("state") == "done")
            )
            if failed:
                sql = f"UPDATE repos SET last_{kind}_at = ? WHERE id = ?"  # noqa: S608
                store.db.execute(sql, (now_ms(), target["id"]))
                store.db.commit()

        if sync is not None:
            result["sync"] = sync
        line = ", ".join(
            f"{k}={v}"
            for k, v in result.items()
            if k
            in (
                "kind",
                "repo",
                "finding",
                "job",
                "state",
                "outcome",
                "skipped",
                "denied",
                "error",
            )
        )
        if sync is not None:
            line = (line + "; " if line else "") + (
                "prsync "
                + "/".join(
                    f"{sync[k]}{k[0]}"
                    for k in (
                        "synced",
                        "merged",
                        "closed",
                        "attention",
                        "errors",
                    )
                )
            )
        store.log_event("cycle", line or str(result))
        return result
    except Exception as e:
        with contextlib.suppress(Exception):
            store.log_event("error", f"cycle crashed: {e!r}")
        return {"error": str(e)}
