"""Triage UI server for the Idle-Token Bug Hunter.

ThreadingHTTPServer + hand-rolled JSON routes; serves ui/index.html.
Thread safety: a fresh Store (own sqlite connection) per request.
Heavy modules (store, budget, scheduler) are imported lazily inside
handlers so the server module stays importable while siblings build.
"""

from __future__ import annotations

import contextlib
import json
import logging
import sys
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import TYPE_CHECKING, Any, ClassVar, Literal, TypedDict, cast
from urllib.parse import parse_qs, urlparse

from pydantic import TypeAdapter

from .types import (
    FINDING_STATUSES,
    REASON_REQUIRED,
    UI_DIR,
    VERDICT_STATUSES,
    Config,
    EventDict,
    JobDict,
    RepoDict,
    Row,
    SchedulerStateDict,
)

if TYPE_CHECKING:
    # Type-only: store/budget/scheduler stay runtime-lazy-imported inside
    # handlers (see module docstring) so this module keeps importing
    # cleanly while siblings build; this import never executes (PEP 563
    # postponed evaluation via `from __future__ import annotations` above
    # means annotations referencing Store are never evaluated at runtime
    # either), so it can't reintroduce that problem -- it only lets mypy
    # replace the Any that used to stand in for Store's real shape below.
    from .store import Store

log = logging.getLogger(__name__)

# One cycle at a time, across all request threads.
_cycle_lock = threading.Lock()
# Wakes the daemon loop early (e.g. budget override set from UI).
_wake = threading.Event()
# PR comments/reviews/merge state cost nothing to check (gh reads only) --
# never let a token-budget backoff also delay noticing PR feedback for
# HOURS. 60s was needlessly tight: sync_prs itself routinely takes
# 30-40s (network calls across every pr_open finding, occasionally
# hitting TLS handshake timeouts), so a 60s cap meant the daemon spent
# most of its time mid-sync and looped a full budget-gated cycle every
# ~90-100s continuously for the entire length of a denial -- for a
# multi-hour backoff, that's dozens of GitHub API sweeps and "denied"
# log lines that taught us nothing new. 5 minutes still satisfies
# "never delayed for hours" while cutting that churn ~5x.
PR_SYNC_INTERVAL_S = 5 * 60.0

# How often the independent usage-prober thread wakes to check
# anthropic:5h staleness (see _usage_prober_loop). Deliberately NOT tied
# to the job-dispatch cadence above (_compute_sleep_s ranges 0s-60min) --
# that coupling is exactly what made a 30min staleness gate capable of
# silently running 90min stale when budget was denied. A short, fixed
# tick keeps "when do we refresh usage data" simple and independently
# reasoned about from "when do we run jobs".
USAGE_PROBE_TICK_S = 60.0


def _reconcile_and_log(store: Store) -> None:
    """Recover jobs/findings orphaned by a previous process dying mid-job
    (crash, systemctl restart, host reboot, or an in-process exception
    run_cycle's own catch-all swallowed). Safe to call at the top of every
    cycle attempt: _cycle_lock guarantees this daemon process is never
    itself mid-run_cycle when this runs, so any 'running' row found here
    cannot belong to still-live work."""
    result = store.reconcile_orphaned_jobs()
    for f in result["findings"]:
        store.log_event(
            "error",
            f"reconciled #{f['id']} stuck 'fixing' -> 'queued' -- prior process died mid-fix",
            finding_id=f["id"],
        )
    for r in result["jobs"]:
        store.log_event(
            "error",
            f"reconciled orphaned {r['kind']} job #{r['id']} "
            f"(finding {r.get('finding_id')}) -- prior process died mid-job",
            job_id=r["id"],
            finding_id=r.get("finding_id"),
        )


class Handler(BaseHTTPRequestHandler):
    cfg: Config  # set by make_server()
    server_version = "hunter/1"
    protocol_version = "HTTP/1.1"
    timeout = 15  # keep-alive timeout: close idle connections after 15s

    # -- plumbing ---------------------------------------------------------

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        log.debug(format, *args)

    def log_error(self, format: str, *args: Any) -> None:  # noqa: A002
        log.warning(format, *args)

    def _store(self) -> Store:
        from .store import Store

        return Store(self.cfg)

    def _send(self, status: int, body: bytes, ctype: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, obj: object, status: int = 200) -> None:
        self._send(status, json.dumps(obj).encode(), "application/json")

    def _error(self, status: int, message: str) -> None:
        self._json({"error": message}, status)

    def _body_json(self) -> Row:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if not raw:
            msg = "empty body"
            raise ValueError(msg)
        obj = json.loads(raw)
        if not isinstance(obj, dict):
            msg = "body must be a JSON object"
            raise TypeError(msg)
        return obj

    # -- GET --------------------------------------------------------------

    _STATIC_TYPES: ClassVar[dict[str, str]] = {
        ".js": "application/javascript; charset=utf-8",
        ".css": "text/css; charset=utf-8",
        ".map": "application/json",
    }

    def do_GET(self) -> None:
        url = urlparse(self.path)
        qs = parse_qs(url.query)
        try:
            if url.path in ("/", "/index.html"):
                page = UI_DIR / "index.html"
                if not page.is_file():
                    self._error(404, "ui/index.html missing")
                    return
                self._send(200, page.read_bytes(), "text/html; charset=utf-8")
                return
            # Static assets (js, css, sourcemaps)
            if not url.path.startswith("/api/"):
                suffix = url.path.rsplit(".", 1)[-1] if "." in url.path else ""
                ctype = self._STATIC_TYPES.get("." + suffix)
                if ctype:
                    asset = UI_DIR / url.path.lstrip("/")
                    if asset.is_file() and UI_DIR in asset.resolve().parents:
                        self._send(200, asset.read_bytes(), ctype)
                        return
            if url.path == "/api/summary":
                self._json(_validate_summary(self._summary()))
                return
            if url.path == "/api/findings":
                self._json(self._findings(qs))
                return
            if url.path == "/api/finding":
                detail = self._finding_detail(qs)
                if detail is None:
                    self._error(404, "no such finding")
                    return
                self._json(detail)
                return
            if url.path == "/api/jobs":
                self._json(self._store().list_jobs(limit=50))
                return
            if url.path == "/api/repos":
                self._json(self._store().list_repos())
                return
            if url.path == "/api/repo/notes":
                self._repo_notes(qs)
                return
            if url.path == "/api/events":
                self._json(self._store().recent_events(limit=100))
                return
            if url.path == "/api/stats":
                store = self._store()
                self._json(
                    {
                        "totals": store.stats_totals(),
                        "by_kind": store.stats_by_kind(),
                        "by_finding": store.stats_by_finding(),
                    }
                )
                return
            self._error(404, "not found")
        except Exception:
            log.exception("GET %s", self.path)
            self._error(500, "internal error")

    def _summary(self) -> SummaryDict:
        from . import budget, scheduler

        store = self._store()
        now_ms = time.time() * 1000
        windows: dict[str, WindowInfoDict] = {}
        for limit_id, w in budget.read_windows().items():
            if ":5h" in limit_id:
                ramp = budget.ramp_5h(w.resets_at, now_ms)
            elif ":7d" in limit_id:
                ramp = budget.ramp_7d(w.resets_at, now_ms)
            else:
                ramp = None
            capacity = store.estimate_capacity(limit_id)
            available_tokens = (
                max(0.0, (ramp if ramp is not None else 1.0) - w.used_fraction) * capacity
                if capacity is not None and w.used_fraction is not None
                else None
            )
            windows[limit_id] = {
                "used_fraction": w.used_fraction,
                "status": w.status,
                "resets_at": w.resets_at,
                "age_s": w.age_s,
                "stale": w.stale,
                "ramp": ramp,
                "available_tokens": available_tokens,
            }
        counts: dict[str, int] = dict.fromkeys(FINDING_STATUSES, 0)
        all_findings = store.list_all_findings()
        counts.update(Counter(f["status"] for f in all_findings))
        # Also add type breakdown
        type_counts = Counter(f["type"] for f in all_findings)
        last_cycle = next(
            (e for e in store.recent_events(limit=500) if e["kind"] == "cycle"),
            None,
        )

        # "What's happening" panel: what's running now, why nothing is
        # (if not), and what's next -- all derived from the SAME
        # functions the scheduler itself uses (pick_next, budget.decide),
        # never a separate guess that could drift from reality.
        current_job = store.current_job()
        next_candidate: NextCandidateDict | None = None
        if current_job is None:
            try:
                picked = scheduler.pick_next(store, self.cfg)
            except Exception:
                picked = None
            if picked is not None:
                kind, target = picked
                is_finding = kind in ("engage", "recheck", "fix")
                budget_kind = "fix" if kind in ("engage", "fix") else "hunt"
                override = target.get("budget_override") if is_finding else None
                if override:
                    budget_state, budget_reason, budget_retry_at = "exempt", f"override: {override}", None
                else:
                    raw_windows = budget.read_windows()
                    repo_id = target["repo_id"] if is_finding else target["id"]
                    dec = budget.decide(
                        cfg=self.cfg,
                        kind=budget_kind,
                        windows=raw_windows,
                        unaccounted=scheduler._unaccounted_tokens(  # noqa: SLF001
                            store,
                            raw_windows,
                            scheduler._anticipated_tokens(store, repo_id, kind),  # noqa: SLF001
                        ),
                    )
                    budget_state = "allowed" if dec.allow else "denied"
                    budget_reason = dec.reason
                    budget_retry_at = dec.retry_at
                next_candidate = {
                    "kind": kind,
                    "id": target["id"],
                    "label": target.get("summary") or target.get("name") or target.get("fingerprint"),
                    "is_finding": is_finding,
                    "budget_state": budget_state,
                    "budget_reason": budget_reason,
                    "budget_retry_at": budget_retry_at,
                }

        scheduler_state = store.get_scheduler_state()
        cycle_running = _cycle_lock.locked()
        return {
            "windows": windows,
            "counts": counts,
            "type_counts": dict(type_counts),
            # store.list_repos() stays Row-typed -- its many OTHER callers
            # (scheduler.py) need the permissive dict[str, Any] shape a
            # RepoDict return type would break. cast(), not
            # _require_keys(), is enough here specifically because
            # _validate_summary (below, at the actual call site) already
            # re-verifies this exact field against RepoDict at runtime,
            # right before serialization -- this cast only needs to
            # satisfy mypy's static view of THIS one field.
            "repos": cast("list[RepoDict]", store.list_repos()),
            "last_cycle": last_cycle,
            "cycle_running": cycle_running,
            "current_job": current_job,
            "next_candidate": next_candidate,
            "scheduler_state": scheduler_state,
            "activity_status": _activity_status(
                current_job, cycle_running, next_candidate, scheduler_state
            ),
        }

    def _repo_notes(self, qs: dict[str, list[str]]) -> None:
        rid_str = (qs.get("id") or [None])[0]  # type: ignore[list-item]
        if not rid_str or not rid_str.isdigit():
            self._error(400, "id query param must be an integer")
            return
        store = self._store()
        repo = store.get_repo(int(rid_str))
        if repo is None:
            self._error(404, f"no repo {rid_str}")
            return
        self._json({"notes": store.repo_notes(repo["id"])})

    def _findings(self, qs: dict[str, list[str]]) -> list[Row]:
        store = self._store()
        status = (qs.get("status") or [None])[0] or None  # type: ignore[list-item]
        repo_key = (qs.get("repo") or [None])[0] or None  # type: ignore[list-item]
        severity = (qs.get("severity") or [None])[0] or None  # type: ignore[list-item]
        finding_type = (qs.get("type") or [None])[0] or None  # type: ignore[list-item]
        unified = (qs.get("unified") or ["1"])[0] == "1"  # default true
        repo_id: int | None = None
        if repo_key is not None:
            key: int | str = int(repo_key) if repo_key.isdigit() else repo_key
            repo = store.get_repo(key)
            if repo is None:
                msg = f"unknown repo {repo_key!r}"
                raise ValueError(msg)
            repo_id = repo["id"]
        
        if unified:
            findings: list[Row] = store.list_all_findings(
                status=status,
                repo_id=repo_id,
                min_severity=severity,
                finding_type=finding_type,
            )
        else:
            # Legacy: bugs only
            findings = store.list_findings(
                status=status,
                repo_id=repo_id,
                min_severity=severity,
            )
        # Embed per-finding event timeline.
        fids: list[int] = [f["id"] for f in findings]
        timelines = store.events_by_finding(fids)
        # Embed pr_state.needs_attention for pr_open findings.
        pr_fids = [f["id"] for f in findings if f["status"] == "pr_open"]
        pr_attention: dict[int, str | None] = {}
        for pfid in pr_fids:
            ps = store.get_pr_state(pfid)
            if ps:
                pr_attention[pfid] = ps.get("needs_attention")
        for f in findings:
            f["timeline"] = timelines.get(f["id"], [])
            if f["id"] in pr_attention:
                f["needs_attention"] = pr_attention[f["id"]]
        return findings

    def _finding_detail(self, qs: dict[str, list[str]]) -> Row | None:
        """Everything about one finding NOT already on its list-view card:
        the full job history (list_jobs()'s /api/jobs feed is capped at
        the most recent 50 across ALL findings, so an older finding's
        jobs can already be gone from it) and PR state (list_findings()
        only ever embeds needs_attention, and only for pr_open -- this
        returns the whole row, for any status a PR could still exist
        under, e.g. merged/rejected). Returns None if id is missing or
        invalid; caller maps that to 400/404."""
        fid_str = (qs.get("id") or [None])[0]  # type: ignore[list-item]
        if not fid_str or not fid_str.isdigit():
            return None
        store = self._store()
        fid = int(fid_str)
        if store.get_finding(fid) is None:
            return None
        return {
            "jobs": store.jobs_by_finding(fid),
            "pr_state": store.get_pr_state(fid),
        }

    # -- POST -------------------------------------------------------------

    def do_POST(self) -> None:
        url = urlparse(self.path)
        try:
            if url.path == "/api/verdict":
                self._verdict()
                return
            if url.path == "/api/cycle":
                self._cycle()
                return
            if url.path == "/api/recheck":
                self._recheck()
                return
            if url.path == "/api/unqueue":
                self._unqueue()
                return
            if url.path == "/api/override":
                self._override()
                return
            if url.path == "/api/repo":
                self._update_repo()
                return
            if url.path == "/api/repos":
                self._add_repo()
                return
            if url.path == "/api/repo/delete":
                self._delete_repo()
                return
            if url.path == "/api/repo/notes":
                self._add_repo_note()
                return
            self._error(404, "not found")
        except (ValueError, json.JSONDecodeError) as exc:
            self._error(400, str(exc))
        except Exception:
            log.exception("POST %s", self.path)
            self._error(500, "internal error")

    def _verdict(self) -> None:
        body = self._body_json()
        fid = body.get("id")
        status = body.get("status")
        reason = (body.get("reason") or "").strip() or None
        if not isinstance(fid, int):
            self._error(400, "id must be an integer")
            return
        if status not in VERDICT_STATUSES:
            self._error(
                400,
                f"status must be one of {list(VERDICT_STATUSES)}",
            )
            return
        if status in REASON_REQUIRED and not reason:
            self._error(400, f"reason required for status {status!r}")
            return
        store = self._store()
        finding = store.get_finding(fid)
        if finding is None:
            self._error(404, f"no finding {fid}")
            return
        store.set_status(fid, status, verdict_reason=reason)
        store.log_event(
            "verdict",
            f"finding {fid} [{finding['fingerprint']}] -> {status}"
            + (f": {reason}" if reason else ""),
            finding_id=fid,
        )
        self._json({"ok": True, "finding": store.get_finding(fid)})

    def _cycle(self) -> None:
        if not _cycle_lock.acquire(blocking=False):
            self._json({"error": "busy"}, 409)
            return
        cfg = self.cfg

        def run() -> None:
            try:
                from . import scheduler
                from .store import Store

                store = Store(cfg)
                _reconcile_and_log(store)
                try:
                    scheduler.run_cycle(store, cfg)
                except Exception:
                    log.exception("cycle failed")
                    with contextlib.suppress(Exception):
                        store.log_event("error", "cycle failed (see logs)")
            finally:
                _cycle_lock.release()

        threading.Thread(target=run, name="hunter-cycle", daemon=True).start()
        self._json({"started": True}, 202)

    def _recheck(self) -> None:
        body = self._body_json()
        fid = body.get("id")
        if not isinstance(fid, int):
            self._error(400, "id must be an integer")
            return
        store = self._store()
        finding = store.get_finding(fid)
        if finding is None:
            self._error(404, f"no finding {fid}")
            return
        if finding["status"] != "new":
            self._error(
                400,
                f"finding #{fid} is {finding['status']!r}, not 'new'",
            )
            return
        store.set_status(fid, "rechecking")
        store.log_event("recheck", f"#{fid} queued for recheck", finding_id=fid)
        self._json({"queued": True, "finding": store.get_finding(fid)})

    def _unqueue(self) -> None:
        body = self._body_json()
        fid = body.get("id")
        if not isinstance(fid, int):
            self._error(400, "id must be an integer")
            return
        store = self._store()
        finding = store.get_finding(fid)
        if finding is None:
            self._error(404, f"no finding {fid}")
            return
        if finding["status"] != "queued":
            self._error(
                400,
                f"finding #{fid} is {finding['status']!r}, not 'queued'",
            )
            return
        store.set_status(fid, "new")
        store.log_event("unqueue", f"#{fid} removed from fix queue", finding_id=fid)
        self._json({"ok": True, "finding": store.get_finding(fid)})

    def _override(self) -> None:
        body = self._body_json()
        fid = body.get("id")
        mode = body.get("mode")  # "once" | "exempt" | None (clear)
        if fid == "all" and mode is None:
            store = self._store()
            n = store.clear_all_overrides()
            store.log_event("override", f"cleared all budget overrides ({n} findings)")
            self._json({"ok": True, "cleared": n})
            return
        if not isinstance(fid, int):
            self._error(400, "id must be an integer (or 'all' with mode=null)")
            return
        if mode not in ("once", "exempt", None):
            self._error(400, "mode must be 'once', 'exempt', or null")
            return
        store = self._store()
        finding = store.get_finding(fid)
        if finding is None:
            self._error(404, f"no finding {fid}")
            return
        store.set_budget_override(fid, mode)
        label = mode or "cleared"
        store.log_event(
            "override",
            f"#{fid} budget override: {label}",
            finding_id=fid,
        )
        self._json({"ok": True, "finding": store.get_finding(fid)})
        if mode:  # setting an override -> wake the daemon loop
            _wake.set()

    def _update_repo(self) -> None:
        body = self._body_json()
        rid = body.get("id")
        if not isinstance(rid, int):
            self._error(400, "id must be an integer")
            return
        store = self._store()
        repo = store.get_repo(rid)
        if repo is None:
            self._error(404, f"no repo {rid}")
            return
        fields: dict[str, str | int] = {}
        if "enabled" in body:
            fields["enabled"] = 1 if body["enabled"] else 0
        if "url" in body and isinstance(body["url"], str) and body["url"].strip():
            fields["url"] = body["url"].strip()
        if "default_branch" in body and isinstance(body["default_branch"], str) and body["default_branch"].strip():
            fields["default_branch"] = body["default_branch"].strip()
        if "forge" in body and body["forge"] in ("github", "gitlab"):
            fields["forge"] = body["forge"]
        if not fields:
            self._error(400, "no valid fields to update")
            return
        store.update_repo(rid, **fields)
        action = ", ".join(f"{k}={v}" for k, v in fields.items())
        store.log_event("repo", f"updated {repo['name']}: {action}")
        self._json({"ok": True, "repo": store.get_repo(rid)})

    def _add_repo(self) -> None:
        from .forge import FORGE_NAMES, detect_forge

        body = self._body_json()
        name = (body.get("name") or "").strip() if isinstance(body.get("name"), str) else ""
        url = (body.get("url") or "").strip() if isinstance(body.get("url"), str) else ""
        if not name or not url:
            self._error(400, "name and url are required")
            return
        branch = body.get("branch")
        branch = branch.strip() if isinstance(branch, str) and branch.strip() else "main"
        forge = body.get("forge") or None
        if forge is None:
            forge = detect_forge(url)
        if forge not in FORGE_NAMES:
            self._error(400, f"unknown forge {forge!r} (choose from {', '.join(FORGE_NAMES)})")
            return
        store = self._store()
        if store.get_repo(name) is not None:
            self._error(409, f"repo {name!r} already exists")
            return
        path = self.cfg.work_root / "repos" / name
        rid = store.add_repo(name, url, str(path), branch, forge=forge)
        store.log_event("repo", f"added {name} ({forge}) -> {path}")
        self._json({"ok": True, "repo": store.get_repo(rid)}, 201)

    def _delete_repo(self) -> None:
        body = self._body_json()
        rid = body.get("id")
        if not isinstance(rid, int):
            self._error(400, "id must be an integer")
            return
        store = self._store()
        repo = store.get_repo(rid)
        if repo is None:
            self._error(404, f"no repo {rid}")
            return
        store.delete_repo(rid)
        store.log_event("repo", f"deleted {repo['name']} (#{rid})")
        self._json({"ok": True})

    def _add_repo_note(self) -> None:
        body = self._body_json()
        rid = body.get("id")
        note = body.get("note")
        category = body.get("category")
        if not isinstance(rid, int):
            self._error(400, "id must be an integer")
            return
        if not isinstance(note, str) or not note.strip():
            self._error(400, "note must be a non-empty string")
            return
        if category is not None and not isinstance(category, str):
            self._error(400, "category must be a string")
            return
        store = self._store()
        repo = store.get_repo(rid)
        if repo is None:
            self._error(404, f"no repo {rid}")
            return
        category = category.strip() or None if category else None
        store.append_repo_note(rid, note.strip(), category=category)
        store.log_event("repo", f"note added to {repo['name']}" + (f" [{category}]" if category else ""))
        self._json({"ok": True, "notes": store.repo_notes(rid)}, 201)


class _Server(ThreadingHTTPServer):
    allow_reuse_address = True  # reuse addr after restart (TIME_WAIT)
    request_queue_size = 64

    def handle_error(
        self,
        request: Any,  # noqa: ARG002
        client_address: tuple[str, int],
    ) -> None:
        log.warning("connection error from %s: %s", client_address, sys.exc_info()[1])


def make_server(cfg: Config, port: int | None = None) -> _Server:
    Handler.cfg = cfg
    addr = ("127.0.0.1", port or cfg.serve_port)
    try:
        httpd = _Server(addr, Handler)
    except OSError as e:
        if e.errno == 98:  # EADDRINUSE
            raise SystemExit(
                f"error: port {addr[1]} already in use"
                " -- is another hunter instance running?\n"
                f"  check: ss -tlnp | grep {addr[1]}\n"
                "  or:    systemctl --user status hunter.service"
            ) from None
        raise
    httpd.daemon_threads = True
    return httpd


def serve(cfg: Config) -> None:
    httpd = make_server(cfg)
    log.info("ui http://127.0.0.1:%d/", httpd.server_address[1])
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        httpd.server_close()


def _describe_cycle(summary: Row) -> tuple[str, str]:
    """(state, detail) for the Status page's "what's happening" panel --
    classifies run_cycle's return shape the same way daemon()'s own
    sleep computation below does, so the displayed reason always matches
    the sleep decision it explains rather than a second, driftable
    interpretation of the same data."""
    if "error" in summary:
        return "error", str(summary["error"])[:200]
    if summary.get("idle"):
        return "idle", str(summary["idle"])
    if summary.get("skipped"):
        return "idle", str(summary["skipped"])
    if summary.get("denied"):
        # Unlike the other branches, this has no "last:" qualifier by
        # default -- reading naturally as "the state right now" next to
        # the pause icon, when it's actually the outcome of whichever
        # cycle last ran (this state can sit unchanged for the whole
        # sleep interval while a fresh preview elsewhere on the page
        # already shows something different, e.g. once the ramp has
        # since caught up). Match the "last: ..." phrasing used below.
        return "denied", f"last: {summary['denied']}"
    state = summary.get("state")
    kind = summary.get("kind")
    if state in ("done", "killed", "failed") and kind:
        who = summary.get("finding")
        target_desc = f"#{who}" if who is not None else f"({summary.get('repo')})"
        outcome = summary.get("outcome")
        bits = f"{kind} {target_desc}"
        if outcome:
            bits += f" -> {outcome}"
        elif state != "done":
            bits += f" {state}"
        return "idle", f"last: {bits}"
    return "idle", "cycle produced no actionable outcome"


class NextCandidateDict(TypedDict):
    """What pick_next()/budget.decide() would do right now, if run --
    the "what's next" preview built fresh in _summary(), never a raw DB
    row (no SQL boundary here, so no runtime check needed: mypy alone
    is sufficient since the construction code below is fully typed)."""

    kind: str
    id: int
    label: str | None
    is_finding: bool
    budget_state: str
    budget_reason: str
    budget_retry_at: float | None


class WindowInfoDict(TypedDict):
    used_fraction: float | None
    status: str | None
    resets_at: int | None
    age_s: float
    stale: bool
    ramp: float | None
    available_tokens: float | None


class _RunningStatus(TypedDict):
    kind: Literal["running"]
    job: JobDict


class _WorkingStatus(TypedDict):
    kind: Literal["working"]


class _ErrorStatus(TypedDict):
    kind: Literal["error"]
    detail: str


class _PausedStatus(TypedDict):
    kind: Literal["paused"]
    candidate: NextCandidateDict


class _ReadyStatus(TypedDict):
    kind: Literal["ready"]
    candidate: NextCandidateDict


class _IdleStatus(TypedDict):
    kind: Literal["idle"]


class _WarmingUpStatus(TypedDict):
    kind: Literal["warming_up"]


ActivityStatus = (
    _RunningStatus
    | _WorkingStatus
    | _ErrorStatus
    | _PausedStatus
    | _ReadyStatus
    | _IdleStatus
    | _WarmingUpStatus
)


class SummaryDict(TypedDict):
    """The complete /api/summary response shape. mypy --strict checks
    every construction site against this (and its nested TypedDicts)
    the same way it does everywhere else in this module -- but that
    only proves the PYTHON CODE THAT BUILDS the dict is internally
    consistent. It says nothing about the actual bytes that leave the
    process: a stale cached client, a manual curl/test hitting this
    route with a monkeypatched Store, or a future bug that slips past
    mypy (a bare Any leaking in somewhere) would all still produce
    whatever shape the code happens to build, unchecked, all the way to
    the wire. _validate_summary is the one place that actually re-
    verifies the real dict against this schema at runtime, at the last
    possible moment before serialization -- the network-boundary half
    of the guarantee _require_keys provides on the SQL-row half."""

    windows: dict[str, WindowInfoDict]
    counts: dict[str, int]
    type_counts: dict[str, int]
    repos: list[RepoDict]
    last_cycle: EventDict | None
    cycle_running: bool
    current_job: JobDict | None
    next_candidate: NextCandidateDict | None
    scheduler_state: SchedulerStateDict | None
    activity_status: ActivityStatus


_SUMMARY_ADAPTER = TypeAdapter(SummaryDict)


def _validate_summary(payload: SummaryDict) -> SummaryDict:
    """Re-verify the built payload against SummaryDict at runtime,
    right before it's serialized -- see SummaryDict's docstring for
    why mypy alone can't provide this guarantee. Raises pydantic's
    ValidationError (caught by do_GET's existing except Exception,
    returning a 500 with the real cause logged) rather than silently
    shipping a malformed payload a client would misinterpret with no
    error at all."""
    return _SUMMARY_ADAPTER.validate_python(payload)


def _activity_status(
    current_job: JobDict | None,
    cycle_running: bool,
    next_candidate: NextCandidateDict | None,
    scheduler_state: SchedulerStateDict | None,
) -> ActivityStatus:
    """The single, canonical answer to "what is hunter doing right now" --
    every rendering surface (the Status page's activity panel AND the
    manual-run button) must derive its text from this function's output
    and nothing else, computed once, server-side, unit-tested.

    Four real incidents in one session came from exactly the opposite
    approach -- each rendering surface independently re-deriving "is
    something happening" from a different subset of the live signals,
    at a different point in time, with no single arbiter: (1) a stale
    "denied" reason shown with no temporal marker, reading as current
    state; (2) a race between _cycle_lock (held immediately) and a job
    row only marked 'running' after real prep-work I/O, so the manual-
    run button and the main panel briefly disagreed; (3) "idle" shown
    for a candidate the budget had already approved; (4) the same
    cycle_running-vs-independent-preview race recurring through the
    new ready/paused states once (3) was fixed. Every fix converged on
    the same lesson: stop computing "what's happening" more than once.
    This function is that one computation, and it's covered by tests
    the way the client-side version it replaced never was.

    ActivityStatus is a discriminated union (Literal "kind" tag, one
    TypedDict per variant carrying only the fields that variant actually
    has -- no "job: None" on a status that was never running) rather
    than one shape with always-present nullable fields: mypy checks each
    return statement against its variant's exact shape, so "attach a job
    to the paused variant" is a type error here, not just a convention.
    The consuming switch in ui/src/app.ts has the equivalent TypeScript
    discriminated union plus an exhaustiveness check, so adding a new
    variant here without updating that switch is a compile error on
    both ends -- the guarantee this whole function exists to provide.

    Priority, highest first -- each check answers "do we know something
    more current than the next one down":
      1. current_job -> "running": we know exactly what's executing.
      2. cycle_running -> "working": _cycle_lock is held (a real cycle
         is picking/deciding/syncing) but no job exists yet -- any
         next_candidate computed independently of that in-flight
         decision is a hypothetical about to be superseded, not fact.
      3. scheduler_state.state == "error" -> "error": worth surfacing
         over a stale/independent candidate preview, but not over
         actually-live current_job/cycle_running above.
      4. next_candidate with budget_state == "denied" -> "paused".
      5. next_candidate otherwise ("allowed" or "exempt") -> "ready":
         nothing is blocking it, it just hasn't been picked up by the
         daemon's wake timer yet -- never "idle", which reads as
         nothing about to happen.
      6. scheduler_state present, nothing above -> "idle": genuinely
         nothing to do.
      7. nothing at all -> "warming_up": no cycle has ever run.
    """
    if current_job is not None:
        return {"kind": "running", "job": current_job}
    if cycle_running:
        return {"kind": "working"}
    if scheduler_state is not None and scheduler_state.get("state") == "error":
        return {"kind": "error", "detail": scheduler_state["detail"]}
    if next_candidate is not None and next_candidate.get("budget_state") == "denied":
        return {"kind": "paused", "candidate": next_candidate}
    if next_candidate is not None:
        return {"kind": "ready", "candidate": next_candidate}
    if scheduler_state is not None:
        return {"kind": "idle"}
    return {"kind": "warming_up"}


def _compute_sleep_s(store: Store, summary: Row) -> float:
    """How long the daemon loop should sleep after this cycle attempt --
    extracted from the loop body so it's directly testable rather than
    only observable by running the real infinite loop.

    Wake policy (smart sleep -- only when necessary):
      - queue has fixes      -> 0s    (drain immediately)
      - just found bugs      -> 5s    (keep momentum)
      - repos exist, no work -> 60s   (periodic check)
      - budget denied        -> dec.retry_at-derived, capped at 60min
      - truly idle (no repos)-> 15min
      - error                -> 5min
      - unrecognized summary shape (e.g. {"idle": ...}, {"skipped": ...})
        -> 15min, the same default the caller started with

    Then, regardless of the above: if a PR sync happened this cycle
    ("sync" in summary), cap at PR_SYNC_INTERVAL_S. sync_prs is free (gh
    reads only, no tokens), so a long token-budget backoff must never
    also delay noticing PR feedback.
    """
    sleep_s: float
    if "error" in summary:
        sleep_s = 5 * 60
    elif summary.get("state") in ("done", "killed", "failed"):
        queued_fixes = store.db.execute(
            "SELECT COUNT(*) FROM findings WHERE status = 'queued'"
        ).fetchone()[0]
        job_produced_findings = summary.get("ingest", {}).get("inserted", 0) > 0
        enabled_repos = store.db.execute(
            "SELECT COUNT(*) FROM repos WHERE enabled = 1"
        ).fetchone()[0]
        if queued_fixes > 0:
            sleep_s = 0
        elif job_produced_findings:
            sleep_s = 5
        elif enabled_repos > 0:
            sleep_s = 60
        else:
            sleep_s = 15 * 60
    elif summary.get("denied"):
        # dec.retry_at (threaded through every "denied" return in
        # scheduler.py) is computed at the source, directly from the
        # WindowState that caused the denial -- an exact answer to
        # "when would this specific denial resolve", not re-derived
        # here from the reason string. None means no informed estimate
        # exists (e.g. missing resets_at); fall back to a generic
        # backoff rather than a value that looks precise but isn't.
        retry_at = summary.get("retry_at")
        if retry_at:
            until_retry = retry_at / 1000 - time.time()
            sleep_s = max(60.0, min(until_retry + 30, 60 * 60))
        else:
            sleep_s = 30 * 60
    else:
        sleep_s = 15 * 60
    if "sync" in summary:
        sleep_s = min(sleep_s, PR_SYNC_INTERVAL_S)
    return sleep_s


def _usage_prober_loop(cfg: Config, stop: threading.Event) -> None:
    """Independent thread: every USAGE_PROBE_TICK_S, force a fresh usage
    probe if anthropic:5h has gone stale.

    See scheduler.refresh_stale_probe's docstring for why this exists
    at all -- headless `omp -p`, everything hunter's workers use, never
    refreshes usage_history on its own, confirmed empirically in
    production.

    Its own thread rather than folded into the job-dispatch loop below
    on purpose: that loop's sleep is intentionally variable (0s-60min,
    backing off when idle or budget-denied so it doesn't busy-loop for
    no reason), but "how fresh is our usage data" has nothing to do
    with "is there a job to run right now" -- a job-cadence backoff
    must never delay this too. Runs once immediately on startup (so a
    window that went stale before the daemon last restarted gets caught
    right away) and every tick thereafter. Best-effort throughout: a
    failed tick just tries again next tick.
    """
    from . import budget, scheduler

    while not stop.is_set():
        try:
            windows = budget.read_windows()
            w5 = windows.get("anthropic:5h")
            if scheduler.refresh_stale_probe(cfg, windows):
                log.info(
                    "usage prober: refreshed a stale anthropic:5h reading (was %.0fs old)",
                    w5.age_s if w5 else float("inf"),
                )
        except Exception:
            log.exception("usage prober tick failed")
        stop.wait(USAGE_PROBE_TICK_S)


def daemon(cfg: Config) -> None:
    """Run forever: UI server + scheduler loop + usage-prober loop, one
    process, three threads.

    The job-dispatch loop shares _cycle_lock with POST /api/cycle, so
    manual and timed cycles never overlap. Idling costs zero tokens --
    every wake goes through the budget gate, which is where all spending
    decisions live. Wake policy: see _compute_sleep_s.

    The usage-prober thread is intentionally separate and unrelated to
    that cadence -- see _usage_prober_loop and USAGE_PROBE_TICK_S.
    """
    import signal as _signal

    httpd = make_server(cfg)
    threading.Thread(target=httpd.serve_forever, name="hunter-ui", daemon=True).start()
    log.info(
        "daemon started: ui http://127.0.0.1:%d/ -- scheduler loop live",
        httpd.server_address[1],
    )

    stop = threading.Event()
    for sig in (_signal.SIGTERM, _signal.SIGINT):
        _signal.signal(sig, lambda *_args: stop.set())

    threading.Thread(
        target=_usage_prober_loop, args=(cfg, stop), name="hunter-usage-prober", daemon=True
    ).start()

    from . import scheduler
    from .store import Store

    while not stop.is_set():
        sleep_s: float = 15 * 60
        if _cycle_lock.acquire(blocking=False):
            _wake.clear()
            try:
                store = Store(cfg)
                _reconcile_and_log(store)
                summary = scheduler.run_cycle(store, cfg)
                sleep_s = _compute_sleep_s(store, summary)
                state_label, detail = _describe_cycle(summary)
                with contextlib.suppress(Exception):
                    store.set_scheduler_state(
                        state_label, detail, int((time.time() + sleep_s) * 1000)
                    )
                log.info(
                    "cycle: %s -> sleep %ds",
                    json.dumps(summary)[:200],
                    sleep_s,
                )
            except Exception as e:
                log.exception("cycle crashed")
                sleep_s = 5 * 60
                if "store" in locals():
                    with contextlib.suppress(Exception):
                        store.set_scheduler_state(
                            "error",
                            f"daemon loop crashed: {e}"[:200],
                            int((time.time() + sleep_s) * 1000),
                        )
            finally:
                _cycle_lock.release()
        else:
            sleep_s = 60  # a UI-triggered cycle is running
            _wake.clear()
        # Wait for stop OR wake, whichever comes first.
        # threading.Event can't OR two events, so poll with short intervals.
        deadline = time.time() + sleep_s
        while not stop.is_set() and not _wake.is_set():
            remaining = deadline - time.time()
            if remaining <= 0:
                break
            stop.wait(min(remaining, 5.0))

    httpd.shutdown()
    httpd.server_close()
    log.info("daemon stopped")
