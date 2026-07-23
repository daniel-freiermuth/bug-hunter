"""Scheduler — one cycle = one job. Fix work drains before new hunts;
run_cycle never raises (the loop that calls it must survive anything).
"""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

from . import budget, runner
from .ingest import ingest_findings
from .playbooks import build_fix_prompt, build_hunt_prompt
from .types import Config, now_ms


def _run(cmd: list[str], cwd=None, timeout: int = 300) -> tuple[int, str]:
    """Run a command; (rc, combined stdout+stderr stripped). Never raises."""
    try:
        p = subprocess.run(
            cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, timeout=timeout,
        )
        return p.returncode, (p.stdout or "").strip()
    except subprocess.TimeoutExpired:
        return 124, f"timeout after {timeout}s: {' '.join(map(str, cmd))}"
    except OSError as e:
        return 127, str(e)


def _ssh_url(url: str) -> str:
    """https://github.com/O/R(.git)? -> git@github.com:O/R.git (ssh passthrough)."""
    m = re.match(r"^https://github\.com/([^/]+)/(.+?)(?:\.git)?/?$", url)
    if m:
        return f"git@github.com:{m.group(1)}/{m.group(2)}.git"
    return url


def _owner_repo(url: str) -> str | None:
    m = re.match(r"^(?:https://github\.com/|git@github\.com:)([^/]+)/(.+?)(?:\.git)?/?$", url)
    return f"{m.group(1)}/{m.group(2)}" if m else None


def _job_state(rr) -> str:
    if rr.killed_reason:
        return "killed"
    return "done" if rr.exit_code == 0 else "failed"


def _record_job(store, job_id: int, rr) -> str:
    state = _job_state(rr)
    fields = dict(
        state=state, tokens_new=rr.tokens_new, calls=rr.calls,
        session_file=rr.session_file, exit_code=rr.exit_code,
        killed_reason=rr.killed_reason, finished_at=now_ms(),
    )
    if state != "done" and rr.stdout_tail:
        fields["notes"] = rr.stdout_tail[-500:]
    store.update_job(job_id, **fields)
    return state


# -- hunt -------------------------------------------------------------------

def run_hunt(store, cfg: Config, repo: dict, force: bool = False) -> dict:
    rid, rname = repo["id"], repo["name"]
    rpath = Path(repo["path"])
    db = repo["default_branch"]

    # Ensure clone + fast-forward to origin's default branch.
    if not rpath.exists():
        rpath.parent.mkdir(parents=True, exist_ok=True)
        rc, out = _run(["git", "clone", repo["url"], str(rpath)], timeout=600)
        if rc != 0:
            store.log_event("error", f"hunt {rname}: clone failed: {out[-300:]}")
            return {"error": f"clone failed: {out[-300:]}"}
    for cmd in (["git", "fetch", "origin"], ["git", "checkout", db],
                ["git", "pull", "--ff-only"]):
        rc, out = _run(["git", "-C", str(rpath), *cmd[1:]], timeout=600)
        if rc != 0:
            store.log_event("error", f"hunt {rname}: {' '.join(cmd)} failed: {out[-300:]}")
            return {"error": f"{' '.join(cmd)} failed: {out[-300:]}"}

    rc, head = _run(["git", "-C", str(rpath), "rev-parse", "HEAD"])
    if rc != 0 or not head:
        store.log_event("error", f"hunt {rname}: rev-parse HEAD failed: {head[-300:]}")
        return {"error": "rev-parse HEAD failed"}

    last = repo.get("last_hunt_sha")
    if last == head and not force:
        store.log_event("hunt", f"{rname}: no new commits since {head[:12]} — skipped")
        return {"skipped": "no new commits", "head": head}

    if last:
        diff_range = f"{last}..{head}"
        scope_note = f"Commits since the last completed hunt ({last[:12]})."
    else:
        rc, base = _run(["git", "-C", str(rpath), "rev-list", "-1",
                         "--before=3 weeks ago", head])
        scope = "the last ~3 weeks of commits"
        if not base or base == head:
            # Quiet repo: a time window is empty — take the last 30 commits.
            rc, base = _run(["git", "-C", str(rpath), "rev-parse", f"{head}~30"])
            scope = "the last 30 commits (repo quiet for >3 weeks)"
            if rc != 0 or not base:
                rc, roots = _run(["git", "-C", str(rpath), "rev-list",
                                  "--max-parents=0", head])
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
        repo, diff_range, scope_note,
        store.suppressions(rid), store.known_active(rid),
        out_path, cfg.hunt_max_findings,
    )
    store.update_job(job, state="running")
    rr = runner.run_worker(cfg, rpath, prompt, dec.cap_tokens, cfg.hunt_max_wall_s,
                           model=cfg.model_for("hunt"))
    state = _record_job(store, job, rr)

    summary: dict = {"kind": "hunt", "repo": rname, "job": job, "state": state,
                     "diff_range": diff_range, "tokens_new": rr.tokens_new,
                     "head": head}
    if out_path.exists():
        counts = ingest_findings(store, rid, out_path)
        summary["ingest"] = counts
        store.log_event(
            "hunt",
            f"{rname}: job {job} {state} over {diff_range[:25]}…"
            f" +{counts['inserted']} new / {counts['duplicates']} dup"
            f" / {counts['invalid']} invalid ({rr.tokens_new} tok)",
            job_id=job,
        )
    else:
        store.log_event("hunt", f"{rname}: job {job} {state}, no findings file"
                        f" ({rr.tokens_new} tok)", job_id=job)
    if state == "done":
        store.set_last_hunt(rid, head)
    return summary


# -- fix --------------------------------------------------------------------

def run_fix(store, cfg: Config, finding: dict) -> dict:
    fid = finding["id"]
    if finding["status"] != "queued":
        return {"skipped": f"finding #{fid} is {finding['status']!r}, not queued"}
    repo = store.get_repo(finding["repo_id"])
    if repo is None:
        store.log_event("error", f"fix #{fid}: repo {finding['repo_id']} missing",
                        finding_id=fid)
        return {"error": "repo missing"}
    rname, rpath, db = repo["name"], repo["path"], repo["default_branch"]

    slug = re.sub(r"[^a-zA-Z0-9]+", "-", finding["summary"]).lower().strip("-")[:40].rstrip("-")
    branch = f"fix/{slug}-{fid}"
    worktree = cfg.work_root / "wt" / f"f{fid}"
    worktree.parent.mkdir(parents=True, exist_ok=True)

    # Retry after a kill/failure: reclaim the salvage worktree and branch —
    # committed proof/fix steps live on the branch, but a fresh worker starts
    # from a clean base (its playbook re-verifies the bug anyway).
    if worktree.exists():
        _run(["git", "-C", rpath, "worktree", "remove", "--force", str(worktree)])
        _run(["git", "-C", rpath, "branch", "-D", branch])
        store.log_event("fix", f"#{fid}: reclaimed stale worktree from prior attempt",
                        finding_id=fid)

    rc, out = _run(["git", "-C", rpath, "worktree", "add", "-b", branch,
                    str(worktree), f"origin/{db}"])
    if rc != 0:
        rc, out = _run(["git", "-C", rpath, "worktree", "add", "-b", branch,
                        str(worktree), db])
    if rc != 0:
        store.log_event("error", f"fix #{fid}: worktree add failed: {out[-300:]}",
                        finding_id=fid)
        return {"error": f"worktree add failed: {out[-300:]}"}

    def _drop_worktree(delete_branch: bool) -> None:
        _run(["git", "-C", rpath, "worktree", "remove", "--force", str(worktree)])
        if delete_branch:
            _run(["git", "-C", rpath, "branch", "-D", branch])

    windows = budget.read_windows()
    dec = budget.decide(cfg, "fix", windows)
    if not dec.allow:
        _drop_worktree(delete_branch=True)
        job = store.create_job("fix", repo["id"], finding_id=fid)
        store.update_job(job, state="denied", notes=dec.reason, finished_at=now_ms())
        store.log_event("deny", f"fix #{fid}: {dec.reason}", job_id=job, finding_id=fid)
        return {"denied": dec.reason, "job": job}

    job = store.create_job("fix", repo["id"], finding_id=fid, cap_tokens=dec.cap_tokens)
    store.set_status(fid, "fixing")
    store.update_job(job, state="running")
    prompt = build_fix_prompt(finding, worktree, branch, repo)
    rr = runner.run_worker(cfg, worktree, prompt, dec.cap_tokens, cfg.fix_max_wall_s,
                           model=cfg.model_for("fix"))
    state = _record_job(store, job, rr)
    summary: dict = {"kind": "fix", "finding": fid, "job": job, "state": state,
                     "branch": branch, "tokens_new": rr.tokens_new}

    # (a) Worker argued innocence.
    not_a_bug = worktree / "NOT-A-BUG.md"
    if not_a_bug.exists():
        reason = not_a_bug.read_text()[:500]
        store.set_status(fid, "rejected", verdict_reason=reason)
        store.log_event("fix", f"#{fid} rejected by worker: {reason.splitlines()[0][:120] if reason else ''}",
                        job_id=job, finding_id=fid)
        _drop_worktree(delete_branch=True)
        summary["outcome"] = "rejected"
        return summary

    # (b) Commits + PR description -> ship a draft PR.
    rc, commits = _run(["git", "-C", str(worktree), "log",
                        f"origin/{db}..HEAD", "--oneline"])
    if rc != 0:
        rc, commits = _run(["git", "-C", str(worktree), "log",
                            f"{db}..HEAD", "--oneline"])
    pr_desc = worktree / "PR-DESCRIPTION.md"
    if rc == 0 and commits and pr_desc.exists():
        push_url = _ssh_url(repo["url"])
        rc, out = _run(["git", "-C", str(worktree), "push", push_url, "HEAD"],
                       timeout=600)
        if rc == 0:
            _, title = _run(["git", "-C", str(worktree), "log", "-1", "--format=%s"])
            slug_or = _owner_repo(repo["url"])
            cmd = ["gh", "pr", "create", "--draft", "--head", branch,
                   "--title", title or branch, "--body-file", str(pr_desc)]
            if slug_or:
                cmd[3:3] = ["-R", slug_or]
            rc, out = _run(cmd, cwd=str(worktree), timeout=300)
            if rc == 0:
                pr_url = out.strip().splitlines()[-1] if out else ""
                store.set_status(fid, "pr_open", pr_url=pr_url)
                store.log_event("ship", f"#{fid} draft PR: {pr_url}",
                                job_id=job, finding_id=fid)
                _drop_worktree(delete_branch=False)
                summary.update(outcome="pr_open", pr_url=pr_url)
                return summary
            failure = f"gh pr create failed: {out[-300:]}"
        else:
            failure = f"push failed: {out[-300:]}"
    else:
        failure = ("no commits" if not commits else "no PR-DESCRIPTION.md") \
            if state == "done" else f"worker {state}"

    # (c) Salvage: requeue, keep the worktree for the next attempt.
    store.set_status(fid, "queued")
    tail = (rr.stdout_tail or "")[-300:]
    store.log_event("fix", f"#{fid} incomplete ({failure}); worktree kept"
                    f" at {worktree}. tail: {tail}", job_id=job, finding_id=fid)
    summary.update(outcome="requeued", failure=failure, worktree=str(worktree))
    return summary


# -- cycle ------------------------------------------------------------------

def run_cycle(store, cfg: Config, force_repo: str | None = None) -> dict:
    try:
        windows = budget.read_windows()
        store.log_window(list(windows.values()))

        queued = store.list_findings(status="queued")
        if queued:
            result = run_fix(store, cfg, queued[-1])  # DESC -> last = oldest
        else:
            repos = [r for r in store.list_repos() if r["enabled"]]
            if force_repo:
                target = store.get_repo(force_repo)
                if target is None:
                    raise ValueError(f"unknown repo {force_repo!r}")
            else:
                target = min(
                    repos,
                    key=lambda r: (r["last_hunt_at"] is not None,
                                   r["last_hunt_at"] or 0),
                ) if repos else None
            if target is not None:
                result = run_hunt(store, cfg, target)
            else:
                result = {"idle": "no queued findings, no enabled repos"}
                store.log_event("cycle", "idle: nothing to do")
                return result

        line = ", ".join(f"{k}={v}" for k, v in result.items()
                         if k in ("kind", "repo", "finding", "job", "state",
                                  "outcome", "skipped", "denied", "error"))
        store.log_event("cycle", line or str(result))
        return result
    except Exception as e:  # noqa: BLE001 — the cycle loop must survive anything
        try:
            store.log_event("error", f"cycle crashed: {e!r}")
        except Exception:
            pass
        return {"error": str(e)}
