"""Scheduler -- one cycle = one job. Fix work drains before new hunts;
run_cycle never raises (the loop that calls it must survive anything).
"""

from __future__ import annotations

import contextlib
import json
import re
from datetime import datetime
from pathlib import Path

from . import budget, runner
from .forge import forge_for
from .ingest import ingest_findings
from .playbooks import (
    build_engage_prompt,
    build_fix_prompt,
    build_hunt_prompt,
    build_recheck_prompt,
)
from .store import Store
from .types import BudgetDecision, Config, Row, RunResult, WindowState, now_ms
from .util import run_cmd


def _job_state(rr: RunResult) -> str:
    if rr.killed_reason:
        return "killed"
    return "done" if rr.exit_code == 0 else "failed"


def _usage_snapshot(windows: dict[str, WindowState]) -> float | None:
    """Max used_fraction across 7d windows, or None if unavailable."""
    fracs = [
        w.used_fraction for k, w in windows.items() if ":7d" in k and w.used_fraction is not None
    ]
    return max(fracs) if fracs else None


def _record_job(
    store: Store,
    job_id: int,
    rr: RunResult,
    model: str | None = None,
    usage_delta: float | None = None,
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
        usage_delta=usage_delta,
        notes=notes,
        finished_at=now_ms(),
    )
    return state


# -- hunt -------------------------------------------------------------------


def run_hunt(store: Store, cfg: Config, repo: Row, force: bool = False) -> Row:
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

    last: str | None = repo.get("last_hunt_sha")
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
    else:
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

    windows = budget.read_windows()
    dec = budget.decide(cfg, "hunt", windows)
    if not dec.allow:
        job = store.create_job("hunt", rid)
        store.update_job(job, state="denied", notes=dec.reason, finished_at=now_ms())
        store.log_event("deny", f"hunt {rname}: {dec.reason}", job_id=job)
        return {"denied": dec.reason, "job": job}

    job = store.create_job("hunt", rid, cap_tokens=dec.cap_tokens)
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
    )
    pre_usage = _usage_snapshot(windows)
    store.update_job(job, state="running")
    model = cfg.model_for("hunt")
    rr = runner.run_worker(
        cfg,
        rpath,
        prompt,
        dec.cap_tokens,
        cfg.hunt_max_wall_s,
        model=model,
    )
    post_usage = _usage_snapshot(budget.read_windows())
    delta = (post_usage - pre_usage) if pre_usage is not None and post_usage is not None else None
    state = _record_job(store, job, rr, model=model, usage_delta=delta)

    summary: Row = {
        "kind": "hunt",
        "repo": rname,
        "job": job,
        "state": state,
        "diff_range": diff_range,
        "tokens_new": rr.tokens_new,
        "head": head,
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
    else:
        store.log_event(
            "hunt",
            f"{rname}: job {job} {state}, no findings file ({rr.tokens_new} tok)",
            job_id=job,
        )
    if state == "done":
        store.set_last_hunt(rid, head)
    return summary


# -- recheck ----------------------------------------------------------------


def run_recheck(store: Store, cfg: Config, finding: Row) -> Row:
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
        store.set_status(fid, "new")
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
            store.set_status(fid, "new")
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
            store.set_status(fid, "new")
            return {"error": f"{' '.join(cmd)} failed: {out[-300:]}"}

    # Budget gate -- recheck is investigative, like hunt.
    override = finding.get("budget_override")
    windows = budget.read_windows()
    if override:
        dec = BudgetDecision(True, f"override:{override}", cfg.hunt_cap_tokens)
    else:
        dec = budget.decide(cfg, "hunt", windows)
    if not dec.allow:
        job = store.create_job("recheck", repo["id"], finding_id=fid)
        store.update_job(job, state="denied", notes=dec.reason, finished_at=now_ms())
        store.set_status(fid, "new")  # un-queue so the button reappears
        store.log_event(
            "deny",
            f"recheck #{fid}: {dec.reason}",
            job_id=job,
            finding_id=fid,
        )
        return {"denied": dec.reason, "job": job}

    job = store.create_job("recheck", repo["id"], finding_id=fid, cap_tokens=dec.cap_tokens)
    out_path = cfg.work_root / "out" / f"recheck{fid}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    prompt = build_recheck_prompt(finding, repo, out_path)
    pre_usage = _usage_snapshot(windows)
    store.update_job(job, state="running")
    model = cfg.model_for("hunt")
    rr = runner.run_worker(
        cfg,
        rpath,
        prompt,
        dec.cap_tokens,
        cfg.hunt_max_wall_s,
        model=model,
    )
    post_usage = _usage_snapshot(budget.read_windows())
    delta = (post_usage - pre_usage) if pre_usage is not None and post_usage is not None else None
    state = _record_job(store, job, rr, model=model, usage_delta=delta)
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
        store.set_status(fid, "new")  # restore -- recheck inconclusive
        store.log_event(
            "recheck",
            f"#{fid}: job {job} {state}, verdict file missing/unparseable",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "failed"
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
        store.log_event(
            "recheck",
            f"#{fid} confirmed: {reason}",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "confirmed"
    elif v == "stale":
        store.set_status(fid, "wontfix", verdict_reason=f"recheck: {reason}")
        store.log_event(
            "recheck",
            f"#{fid} stale: {reason}",
            job_id=job,
            finding_id=fid,
        )
        summary["outcome"] = "stale"
    elif v == "invalid":
        store.set_status(fid, "rejected", verdict_reason=f"recheck: {reason}")
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

# -- fix --------------------------------------------------------------------


def run_fix(store: Store, cfg: Config, finding: Row) -> Row:
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

    slug = re.sub(r"[^a-zA-Z0-9]+", "-", finding["summary"]).lower().strip("-")[:40].rstrip("-")
    branch = f"fix/{slug}-{fid}"
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
    windows = budget.read_windows()
    if override:
        base = cfg.fix_cap_tokens
        dec = BudgetDecision(True, f"override:{override}", base)
    else:
        dec = budget.decide(cfg, "fix", windows)
    if not dec.allow:
        _drop_worktree(delete_branch=True)
        job = store.create_job("fix", repo["id"], finding_id=fid)
        store.update_job(job, state="denied", notes=dec.reason, finished_at=now_ms())
        store.log_event(
            "deny",
            f"fix #{fid}: {dec.reason}",
            job_id=job,
            finding_id=fid,
        )
        return {"denied": dec.reason, "job": job}

    job = store.create_job("fix", repo["id"], finding_id=fid, cap_tokens=dec.cap_tokens)
    pre_usage = _usage_snapshot(windows)
    store.set_status(fid, "fixing")
    store.update_job(job, state="running")
    prompt = build_fix_prompt(finding, worktree, branch, repo)
    model = cfg.model_for("fix")
    rr = runner.run_worker(
        cfg,
        worktree,
        prompt,
        dec.cap_tokens,
        cfg.fix_max_wall_s,
        model=model,
    )
    post_usage = _usage_snapshot(budget.read_windows())
    delta = (post_usage - pre_usage) if pre_usage is not None and post_usage is not None else None
    state = _record_job(store, job, rr, model=model, usage_delta=delta)
    summary: Row = {
        "kind": "fix",
        "finding": fid,
        "job": job,
        "state": state,
        "branch": branch,
        "tokens_new": rr.tokens_new,
    }

    # (a) Worker argued innocence.
    not_a_bug = worktree / "NOT-A-BUG.md"
    if not_a_bug.exists():
        reason = not_a_bug.read_text()[:500]
        store.set_status(fid, "rejected", verdict_reason=reason)
        first_line = reason.splitlines()[0][:120] if reason else ""
        store.log_event(
            "fix",
            f"#{fid} rejected by worker: {first_line}",
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

    # (c) Salvage: requeue, keep the worktree for the next attempt.
    store.set_status(fid, "queued")
    tail = (rr.stdout_tail or "")[-300:]
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
) -> tuple[str | None, bool]:
    """(short human summary, any_failing) from a statusCheckRollup list."""
    if not rollup:
        return None, False
    concl = [(c.get("conclusion") or c.get("state") or "").upper() for c in rollup]
    failing = sum(1 for c in concl if c in _FAIL_CONCLUSIONS)
    passing = sum(1 for c in concl if c in ("SUCCESS", "NEUTRAL", "SKIPPED"))
    pending = len(concl) - failing - passing
    parts = [f"{passing} pass"]
    if failing:
        parts.append(f"{failing} fail")
    if pending:
        parts.append(f"{pending} pending")
    return " / ".join(parts), failing > 0


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
        checks, failing = _checks_summary(pr.get("statusCheckRollup"))
        if prev is None or prev.get("last_engaged_activity_at") is None:
            # First sync: the PR-creation chatter is our own -- baseline the
            # watermark at the PR's current activity without flagging.
            engaged = max(_iso_ms(pr.get("updatedAt")), last_activity)
        else:
            engaged = prev["last_engaged_activity_at"]

        reasons: list[str] = []
        if last_activity > (engaged or 0):
            reasons.append("new_comments")
        if (pr.get("reviewDecision") or "").upper() == "CHANGES_REQUESTED":
            reasons.append("changes_requested")
        if (pr.get("mergeable") or "").upper() == "CONFLICTING":
            reasons.append("conflict")
        if failing:
            reasons.append("checks_failing")
        attention = ",".join(reasons) or None

        store.upsert_pr_state(
            fid,
            pr_number=num,
            state=pr_state,
            mergeable=pr.get("mergeable"),
            checks=checks,
            head_ref=pr.get("headRefName"),
            last_activity_at=last_activity,
            last_engaged_activity_at=engaged,
            needs_attention=attention,
            synced_at=now_ms(),
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


def run_engage(store: Store, cfg: Config, finding: Row) -> Row:
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
    windows = budget.read_windows()
    if override:
        dec = BudgetDecision(True, f"override:{override}", cfg.fix_cap_tokens)
    else:
        dec = budget.decide(cfg, "fix", windows)
    if not dec.allow:
        _drop_worktree()
        job = store.create_job("engage", repo["id"], finding_id=fid)
        store.update_job(job, state="denied", notes=dec.reason, finished_at=now_ms())
        store.log_event(
            "deny",
            f"engage #{fid}: {dec.reason}",
            job_id=job,
            finding_id=fid,
        )
        return {"denied": dec.reason, "job": job}

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
        cap_tokens=dec.cap_tokens,
    )
    store.update_job(job, state="running")
    prompt = build_engage_prompt(
        worktree,
        head_ref,
        repo,
        pr,
        ps.get("needs_attention") or "",
    )
    pre_usage = _usage_snapshot(windows)
    model = cfg.model_for("fix")
    rr = runner.run_worker(
        cfg,
        worktree,
        prompt,
        dec.cap_tokens,
        cfg.fix_max_wall_s,
        model=model,
    )
    post_usage = _usage_snapshot(budget.read_windows())
    delta = (post_usage - pre_usage) if pre_usage is not None and post_usage is not None else None
    state = _record_job(store, job, rr, model=model, usage_delta=delta)
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
    store.upsert_pr_state(
        fid,
        last_engaged_activity_at=engaged_mark,
        needs_attention=None,
        synced_at=now_ms(),
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


# -- cycle ------------------------------------------------------------------


def run_cycle(store: Store, cfg: Config, force_repo: str | None = None) -> Row:
    try:
        windows = budget.read_windows()
        store.log_window(list(windows.values()))

        # (0) Cheap PR sync -- gh reads only, no tokens.
        sync: Row | None = sync_prs(store, cfg) if store.list_findings(status="pr_open") else None

        rechecking = store.list_findings(status="rechecking")
        attention = store.list_attention()
        queued = store.list_findings(status="queued")

        # Budget-overridden findings jump the queue across all categories.
        override_target: tuple[str, Row] | None = None
        for kind, items in (("engage", attention), ("recheck", rechecking), ("fix", queued)):
            for f in items:
                if f.get("budget_override"):
                    override_target = (kind, f)
                    break
            if override_target:
                break

        if override_target:
            kind, f = override_target
            if kind == "engage":
                result = run_engage(store, cfg, f)
            elif kind == "recheck":
                result = run_recheck(store, cfg, f)
            else:
                result = run_fix(store, cfg, f)
        elif attention:
            result = run_engage(store, cfg, attention[0])  # stalest sync first
        elif rechecking:
            result = run_recheck(store, cfg, rechecking[-1])  # DESC -> last = oldest
        elif queued:
            result = run_fix(store, cfg, queued[-1])  # DESC -> last = oldest
        else:
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
                        key=lambda r: (
                            r["last_hunt_at"] is not None,
                            r["last_hunt_at"] or 0,
                        ),
                    )
                    if repos
                    else None
                )
            if target is not None:
                result = run_hunt(store, cfg, target)
            else:
                result: Row = {  # type: ignore[no-redef]
                    "idle": "no queued findings, no enabled repos",
                }
                if sync is not None:
                    result["sync"] = sync
                store.log_event("cycle", "idle: nothing to do")
                return result

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
