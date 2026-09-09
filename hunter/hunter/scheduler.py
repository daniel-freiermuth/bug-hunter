"""Scheduler -- one cycle = one job. Fix work drains before new hunts;
run_cycle never raises (the loop that calls it must survive anything).
"""

from __future__ import annotations

import contextlib
import json
import re
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

from .backend import Backend, Denied, Granted, JobClass, Outlook
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


def anticipated_tokens(store: Store, cfg: Config, repo_id: int, kind: str) -> int:
    """Realistic anticipated cost of the job about to be decided on -- not
    its nominal cap_tokens. A cold prompt-cache first call for a given
    (repo, kind) pair can cost 2-5x cap_tokens in one atomic LLM call the
    runner's watchdog cannot interrupt mid-flight (see runner.py): sizing
    the pre-start reservation on cap_tokens systematically under-estimates
    exactly the jobs most likely to blow through it (observed in
    production: dep_update/refactor jobs cold-starting at 2-4x their
    200k cap while the scheduler's own accounting still assumed 200k).

    If this exact (repo, kind) pair has finished within the configured
    cache TTL (cfg.cache_ttl_s, default 1h -- Anthropic's observed prompt-
    cache lifetime), its prompt cache is probably still warm -- anticipate
    anticipate its historical p90: a cold cache-write is likely, and a
    handful of jobs having been merely cheap doesn't mean this one will be.
    No history for this kind yet -> nothing to anticipate beyond whatever
    cap_tokens/inflight accounting already covers.
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
    history = [
        r["tokens_new"]
        for r in store.db.execute(
            "SELECT tokens_new FROM jobs WHERE kind = ? AND tokens_new IS NOT NULL"
            " ORDER BY tokens_new",
            (kind,),
        ).fetchall()
    ]
    if not history:
        return 0
    idx = min(int(len(history) * (0.5 if warm else 0.9)), len(history) - 1)
    return history[idx]


def _job_state(rr: RunResult) -> str:
    if rr.killed_reason:
        return "killed"
    return "done" if rr.exit_code == 0 else "failed"



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
    counts = ingest_findings(store, repo_id, followups_path, finding_type=None)
    if counts["inserted"]:
        store.log_event(
            kind,
            f"#{fid}: +{counts['inserted']} follow-up(s) filed from deferred/superseded work"
            f" ({counts['duplicates']} dup / {counts['invalid']} invalid)",
            job_id=job,
            finding_id=fid,
        )


# -- hunt -------------------------------------------------------------------


def run_hunt(store: Store, cfg: Config, repo: Row, backend: Backend, force: bool = False) -> Row:
    rid: int = repo["id"]
    rname: str = repo["name"]
    rpath = Path(repo["path"])
    db: str = repo["default_branch"]

    # Ensure clone + fast-forward to origin's default branch.
    if not rpath.exists():
        rpath.parent.mkdir(parents=True, exist_ok=True)
        rc, out = run_cmd(["git", "clone", repo["url"], str(rpath)], timeout=600)
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
    
    if rehunt_due and not force:
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

    if last == head and not force:
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
        EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
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

    cfg_cap = cfg.hunt_cap_tokens
    outlook = backend.decide(anticipated_tokens=anticipated_tokens(store, cfg, rid, "hunt"))
    verdict = outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            job = store.create_job("hunt", rid)
            store.update_job(job, state="denied", notes=reason, finished_at=now_ms())
            store.log_event("deny", f"hunt {rname}: {reason}", job_id=job)
            return {"denied": reason, "retry_at": retry_at, "job": job}
        case Granted(cap_tokens=backend_cap):
            pass
    cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap
    job = store.create_job("hunt", rid, cap_tokens=cap, state="running")
    out_path = cfg.work_root / "out" / f"job{job}.findings.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    prompt = build_hunt_prompt(
        repo,
        diff_range,
        scope_note,
        store.suppressions(rid),
        store.known_active(rid),
        out_path,
        cfg.hunt_max_findings,
        store.repo_notes(rid),
    )
    model = cfg.model_for("hunt")
    rr = backend.run(rpath, prompt, cap_tokens=cap, max_wall_s=cfg.hunt_max_wall_s, job_class=JobClass.HUNT)
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
    if out_path.exists():
        counts = ingest_findings(store, rid, out_path)
        summary["ingest"] = counts
        store.log_event(
            "hunt",
            f"{rname}: job {job} {state} over {diff_range[:25]}..."
            f" +{counts['inserted']} new / {counts['duplicates']} dup"
            f" / {counts['invalid']} invalid ({rr.tokens_new} tok)",
            job_id=job,
        )
        # Only update watermarks when output is produced
        if state == "done":
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


def run_recheck(store: Store, cfg: Config, finding: Row, backend: Backend) -> Row:
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

    # Ensure clone + fast-forward to latest default branch.
    if not rpath.exists():
        rpath.parent.mkdir(parents=True, exist_ok=True)
        rc, out = run_cmd(["git", "clone", repo["url"], str(rpath)], timeout=600)
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
    cfg_cap = cfg.hunt_cap_tokens
    outlook = backend.decide(anticipated_tokens=anticipated_tokens(store, cfg, repo["id"], "recheck"))
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            job = store.create_job("recheck", repo["id"], finding_id=fid)
            store.update_job(job, state="denied", notes=reason, finished_at=now_ms())
            store.log_event(
                "deny",
                f"recheck #{fid}: {reason}",
                job_id=job,
                finding_id=fid,
            )
            return {"denied": reason, "retry_at": retry_at, "job": job}
        case Granted(cap_tokens=backend_cap):
            pass
    cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap
    job = store.create_job("recheck", repo["id"], finding_id=fid, cap_tokens=cap, state="running")
    out_path = cfg.work_root / "out" / f"recheck{fid}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    prompt = build_recheck_prompt(finding, repo, out_path, store.repo_notes(repo["id"]))
    model = cfg.model_for("hunt")
    rr = backend.run(rpath, prompt, cap_tokens=cap, max_wall_s=cfg.hunt_max_wall_s, job_class=JobClass.HUNT)
    state = _record_job(store, job, rr, model=model)
    summary: Row = {
        "kind": "recheck",
        "finding": fid,
        "job": job,
        "state": state,
        "tokens_new": rr.tokens_new,
    }

    # Post-process verdict file.
    verdict: Row | None = None
    if out_path.exists():
        try:
            verdict = json.loads(out_path.read_text())
        except (OSError, json.JSONDecodeError):
            verdict = None

    if not isinstance(verdict, dict) or verdict.get("verdict") not in (
        "confirmed",
        "stale",
        "invalid",
    ):
        if state != "done":
            failure = f"worker {state}"
        elif verdict is None and not out_path.exists():
            failure = "no verdict file"
        elif verdict is None:
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

    v: str = verdict["verdict"]
    reason = (verdict.get("reason") or "")[:500]

    if v == "confirmed":
        store.update_finding_analysis(
            fid,
            summary=verdict.get("updated_summary"),
            detail=verdict.get("updated_detail"),
            confidence=verdict.get("updated_confidence"),
            severity=verdict.get("updated_severity"),
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
    prompt_builder: Callable[
        [Row, str, list[Row], list[Row], Path, int, str], str
    ]


def _run_analysis_job(store: Store, cfg: Config, repo: Row, spec: _AnalysisSpec, backend: Backend) -> Row:
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
    cfg_cap = cfg.hunt_cap_tokens
    outlook = backend.decide(anticipated_tokens=anticipated_tokens(store, cfg, rid, kind))
    verdict = outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            job = store.create_job(kind, rid)
            store.update_job(job, state="denied", notes=reason, finished_at=now_ms())
            store.log_event("deny", f"{kind} {rname}: {reason}", job_id=job)
            return {"denied": reason, "retry_at": retry_at, "job": job}
        case Granted(cap_tokens=backend_cap):
            pass
    cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap
    job = store.create_job(kind, rid, cap_tokens=cap, state="running")
    out_path = cfg.work_root / "out" / f"job{job}.{spec.out_plural}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)

    suppressions = store.suppressions(rid, finding_type=kind)
    known = store.known_active(rid, finding_type=kind)
    prompt = spec.prompt_builder(
        repo,
        spec.scope_note,
        suppressions,
        known,
        out_path,
        cfg.hunt_max_findings,  # reuse hunt max for now
        store.repo_notes(rid),
    )

    model = cfg.model_for("hunt")
    rr = backend.run(rpath, prompt, cap_tokens=cap, max_wall_s=cfg.hunt_max_wall_s, job_class=JobClass.HUNT)
    state = _record_job(store, job, rr, model=model)

    summary: Row = {
        "kind": kind,
        "repo": rname,
        "job": job,
        "state": state,
        "tokens_new": rr.tokens_new,
    }

    if out_path.exists():
        counts = ingest_findings(store, rid, out_path, finding_type=kind)
        summary["ingest"] = counts
        store.log_event(
            kind,
            f"{rname}: job {job} {state} -- +{counts['inserted']} new / {counts['duplicates']} dup"
            f" / {counts['invalid']} invalid ({rr.tokens_new} tok)",
            job_id=job,
        )
        # Only update timestamp after successful output + ingestion
        if state == "done":
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
        "Scan for safe, mechanical refactoring opportunities"
        " (duplication, dead code, complexity)."
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


def run_test_gap(store: Store, cfg: Config, repo: Row, backend: Backend) -> Row:
    """Hunt for test coverage gaps in a repo."""
    return _run_analysis_job(store, cfg, repo, _TEST_GAP_SPEC, backend)


def run_dep_update(store: Store, cfg: Config, repo: Row, backend: Backend) -> Row:
    """Check for outdated dependencies."""
    return _run_analysis_job(store, cfg, repo, _DEP_UPDATE_SPEC, backend)


def run_refactor(store: Store, cfg: Config, repo: Row, backend: Backend) -> Row:
    """Hunt for mechanical refactoring opportunities."""
    return _run_analysis_job(store, cfg, repo, _REFACTOR_SPEC, backend)


def run_modernize(store: Store, cfg: Config, repo: Row, backend: Backend) -> Row:
    """Hunt for SOTA-drift modernization opportunities -- deprecated or
    unmaintained dependencies, language-feature gaps, format/protocol
    shifts, major version debt, platform EOL. Explicitly NOT bounded to
    safe/mechanical changes like refactor/dep_update; see modernization.md."""
    return _run_analysis_job(store, cfg, repo, _MODERNIZATION_SPEC, backend)


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


def run_fix(store: Store, cfg: Config, finding: Row, backend: Backend) -> Row:
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
    cfg_cap = cfg.fix_cap_tokens
    outlook = backend.decide(anticipated_tokens=anticipated_tokens(store, cfg, repo["id"], "fix"))
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            _drop_worktree(delete_branch=True)
            job = store.create_job("fix", repo["id"], finding_id=fid)
            store.update_job(job, state="denied", notes=reason, finished_at=now_ms())
            store.log_event(
                "deny",
                f"fix #{fid}: {reason}",
                job_id=job,
                finding_id=fid,
            )
            return {"denied": reason, "retry_at": retry_at, "job": job}
        case Granted(cap_tokens=backend_cap):
            pass
    cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap
    job = store.create_job("fix", repo["id"], finding_id=fid, cap_tokens=cap, state="running")
    with store.in_progress(fid, "fixing", fallback="queued"):
        build_prompt = (
            build_fix_prompt
            if is_bug
            else build_apply_modernization_prompt
            if is_modernization
            else build_apply_improvement_prompt
        )
        prompt = build_prompt(finding, worktree, branch, repo, store.repo_notes(repo["id"]))
        model = cfg.model_for("fix")
        rr = backend.run(worktree, prompt, cap_tokens=cap, max_wall_s=cfg.fix_max_wall_s, job_class=JobClass.FIX)
        state = _record_job(store, job, rr, model=model)
        summary: Row = {
            "kind": "fix",
            "finding": fid,
            "job": job,
            "state": state,
            "branch": branch,
            "tokens_new": rr.tokens_new,
        }

        # (a) Worker declined this finding, or hit a blocker.
        decline_file = worktree / ("NOT-A-BUG.md" if is_bug else "DECLINED.md")
        blocked_file = worktree / "BLOCKED.md"
        outcome_file = decline_file if decline_file.exists() else (blocked_file if blocked_file.exists() else None)
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
                    f"stuck: {streak} consecutive fix attempts hit the same"
                    f" failure: {failure}"
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


def run_engage(store: Store, cfg: Config, finding: Row, backend: Backend) -> Row:
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
    cfg_cap = cfg.fix_cap_tokens
    outlook = backend.decide(anticipated_tokens=anticipated_tokens(store, cfg, repo["id"], "engage"))
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            _drop_worktree()
            job = store.create_job("engage", repo["id"], finding_id=fid)
            store.update_job(job, state="denied", notes=reason, finished_at=now_ms())
            store.log_event(
                "deny",
                f"engage #{fid}: {reason}",
                job_id=job,
                finding_id=fid,
            )
            return {"denied": reason, "retry_at": retry_at, "job": job}
        case Granted(cap_tokens=backend_cap):
            pass
    cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap

    rc, pr, raw = forge.view_pr_engage(owner_slug, num)
    if pr is None:
        store.log_event(
            "error",
            f"engage #{fid}: PR/MR view failed: {(raw or '')[-300:]}",
            finding_id=fid,
        )
        _drop_worktree()
        return {"error": "PR/MR view failed"}

    job = store.create_job(
        "engage",
        repo["id"],
        finding_id=fid,
        cap_tokens=cap,
        state="running",
    )
    prompt = build_engage_prompt(
        worktree,
        head_ref,
        repo,
        pr,
        ps.get("needs_attention") or "",
        store.repo_notes(repo["id"]),
    )
    model = cfg.model_for("fix")
    rr = backend.run(worktree, prompt, cap_tokens=cap, max_wall_s=cfg.fix_max_wall_s, job_class=JobClass.FIX)
    state = _record_job(store, job, rr, model=model)
    summary: Row = {
        "kind": "engage",
        "finding": fid,
        "job": job,
        "state": state,
        "pr": num,
        "tokens_new": rr.tokens_new,
    }

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


def run_harvest(store: Store, cfg: Config, finding: Row, backend: Backend) -> Row:
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
    if worktree.exists():
        run_cmd(["git", "-C", rpath, "worktree", "remove", "--force", str(worktree)])
        store.log_event(
            "harvest",
            f"#{fid}: reclaimed stale worktree from prior attempt",
            finding_id=fid,
        )

    def _drop_worktree() -> None:
        run_cmd(["git", "-C", rpath, "worktree", "remove", "--force", str(worktree)])

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
    cfg_cap = cfg.fix_cap_tokens
    outlook = backend.decide(anticipated_tokens=anticipated_tokens(store, cfg, repo["id"], "harvest"))
    verdict = outlook.prioritized if override else outlook.normal
    match verdict:
        case Denied(reason=reason, retry_at=retry_at):
            _drop_worktree()
            job = store.create_job("harvest", repo["id"], finding_id=fid)
            store.update_job(job, state="denied", notes=reason, finished_at=now_ms())
            store.log_event("deny", f"harvest #{fid}: {reason}", job_id=job, finding_id=fid)
            return {"denied": reason, "retry_at": retry_at, "job": job}
        case Granted(cap_tokens=backend_cap):
            pass
    cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap

    rc, pr, raw = forge.view_pr_engage(owner_slug, num)
    if pr is None:
        store.log_event(
            "error",
            f"harvest #{fid}: PR/MR view failed: {(raw or '')[-300:]}",
            finding_id=fid,
        )
        _drop_worktree()
        return {"error": "PR/MR view failed"}

    job = store.create_job(
        "harvest", repo["id"], finding_id=fid, cap_tokens=cap, state="running"
    )
    prompt = build_harvest_prompt(finding, worktree, repo, pr, num, store.repo_notes(repo["id"]))
    model = cfg.model_for("fix")
    rr = backend.run(worktree, prompt, cap_tokens=cap, max_wall_s=cfg.fix_max_wall_s, job_class=JobClass.FIX)
    state = _record_job(store, job, rr, model=model)

    # Read before _drop_worktree below removes the file.
    _ingest_followups(store, repo["id"], worktree, fid, job, "harvest")
    _drop_worktree()

    summary: Row = {"kind": "harvest", "finding": fid, "job": job, "state": state, "pr": num}
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


def pick_next(
    store: Store, cfg: Config, force_repo: str | None = None  # noqa: ARG001
) -> tuple[str, Row] | None:
    """Pure selection: what run_cycle would act on right now, if invoked --
    no side effects, no budget check (each run_* function evaluates its
    own budget when actually executed; this only answers "what", not
    "would it currently be allowed"). This is the single place the
    priority order is expressed; run_cycle and the Status page's
    "what's next" preview both call it, so the preview can never drift
    from what actually runs.

    Priority: a budget-overridden finding (any category) jumps the
    queue -> flagged PR (oldest-outstanding reason first) -> oldest merged PR pending
    follow-up review (oldest merge first) -> oldest rechecking -> oldest
    queued fix -> the most stale-of-rotation job type for the
    least-recently-hunted enabled repo (hunt if never cloned, else
    whichever of hunt/test_gap/dep_update/refactor is oldest/never-run;
    modernization joins that pool too, but only once
    cfg.modernization_interval_days have passed since it last ran for
    this repo -- it's a periodic strategic check, not a tight-loop scan,
    so it must not compete for scan slots against the other four every
    single cycle).

    Returns (kind, target): target is a finding row for
    engage/harvest/recheck/fix, a repo row for
    hunt/test_gap/dep_update/refactor/modernization. None means nothing
    to do (no queued/attention/pending-harvest/rechecking work and no
    enabled repos).
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
                return kind, f

    if attention:
        return "engage", attention[0]  # oldest-flagged reason first (see list_attention)
    if pending_harvest:
        return "harvest", pending_harvest[0]  # oldest merge first
    if rechecking:
        return "recheck", rechecking[-1]  # DESC -> last = oldest
    if queued:
        return "fix", queued[-1]  # DESC -> last = oldest

    repos = [r for r in store.list_repos() if r["enabled"]]
    if force_repo:
        target = store.get_repo(force_repo)
        if target is None:
            msg = f"unknown repo {force_repo!r}"
            raise ValueError(msg)
    else:
        target = (
            min(
                repos,
                key=lambda r: (r["last_hunt_at"] is not None, r["last_hunt_at"] or 0),
            )
            if repos
            else None
        )
    if target is None:
        return None

    rpath = Path(target["path"])
    if not rpath.exists():
        return "hunt", target  # not cloned yet -> hunt does the clone

    job_times = {
        "hunt": target.get("last_hunt_at") or 0,
        "test_gap": target.get("last_test_gap_at") or 0,
        "dep_update": target.get("last_dep_update_at") or 0,
        "refactor": target.get("last_refactor_at") or 0,
    }
    last_modernization = target.get("last_modernization_at") or 0
    interval_ms = cfg.modernization_interval_days * 86_400_000
    if last_modernization == 0 or (now_ms() - last_modernization) >= interval_ms:
        job_times["modernization"] = last_modernization
    never_run = [k for k, v in job_times.items() if v == 0]
    job_type = (
        next(k for k in _JOB_TYPE_PRIORITY if k in never_run)
        if never_run
        else min(job_times, key=job_times.get)
    )
    return job_type, target


_RUNNERS: dict[str, Callable[[Store, Config, Row, Backend], Row]] = {
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
        # (0) Cheap PR sync -- gh reads only, no tokens.
        sync: Row | None = sync_prs(store, cfg) if store.list_findings(status="pr_open") else None

        picked = pick_next(store, cfg, force_repo)
        if picked is None:
            result: Row = {"idle": "no queued findings, no enabled repos"}
            if sync is not None:
                result["sync"] = sync
            store.log_event("cycle", "idle: nothing to do")
            return result

        kind, target = picked
        result = _RUNNERS[kind](store, cfg, target, backend)

        # Prevent starvation of repo-level rotation jobs: if this attempt
        # didn't succeed (failed, killed, or done without output), still
        # bump the timestamp so this job type stops being the perpetual
        # "oldest" pick. Without this, a job type that succeeded once and
        # then fails persistently keeps a frozen-old timestamp while its
        # siblings' timestamps advance past it on their own successes --
        # min()-based fairness then re-selects the stuck job type every
        # cycle forever, starving hunt/test_gap/dep_update/refactor/
        # modernization of their turns. hunt has its own watermark logic
        # (set_last_hunt) and is exempt.
        if kind in ("test_gap", "dep_update", "refactor", "modernization"):
            failed = (
                result.get("state") in ("killed", "failed")
                or result.get("error")
                or result.get("ingest_error")
                or (result.get("state") == "done" and "ingest" not in result)
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

