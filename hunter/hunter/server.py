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
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, ClassVar
from urllib.parse import parse_qs, urlparse

from .types import FINDING_STATUSES, REASON_REQUIRED, UI_DIR, VERDICT_STATUSES, Config, Row

log = logging.getLogger(__name__)

# One cycle at a time, across all request threads.
_cycle_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    cfg: Config  # set by make_server()
    server_version = "hunter/1"
    protocol_version = "HTTP/1.1"

    # -- plumbing ---------------------------------------------------------

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        log.debug(format, *args)

    def _store(self) -> Any:
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
                self._json(self._summary())
                return
            if url.path == "/api/findings":
                self._json(self._findings(qs))
                return
            if url.path == "/api/jobs":
                self._json(self._store().list_jobs(limit=50))
                return
            if url.path == "/api/repos":
                self._json(self._store().list_repos())
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

    def _summary(self) -> Row:
        from . import budget

        windows: Row = {}
        for limit_id, w in budget.read_windows().items():
            windows[limit_id] = {
                "used_fraction": w.used_fraction,
                "status": w.status,
                "resets_at": w.resets_at,
                "age_s": w.age_s,
                "stale": w.stale,
            }
        store = self._store()
        counts: dict[str, int] = dict.fromkeys(FINDING_STATUSES, 0)
        counts.update(Counter(f["status"] for f in store.list_findings()))
        last_cycle = next(
            (e for e in store.recent_events(limit=500) if e["kind"] == "cycle"),
            None,
        )
        return {
            "windows": windows,
            "counts": counts,
            "repos": store.list_repos(),
            "last_cycle": last_cycle,
            "cycle_running": _cycle_lock.locked(),
        }

    def _findings(self, qs: dict[str, list[str]]) -> list[Row]:
        store = self._store()
        status = (qs.get("status") or [None])[0] or None  # type: ignore[list-item]
        repo_key = (qs.get("repo") or [None])[0] or None  # type: ignore[list-item]
        severity = (qs.get("severity") or [None])[0] or None  # type: ignore[list-item]
        repo_id: int | None = None
        if repo_key is not None:
            key: int | str = int(repo_key) if repo_key.isdigit() else repo_key
            repo = store.get_repo(key)
            if repo is None:
                msg = f"unknown repo {repo_key!r}"
                raise ValueError(msg)
            repo_id = repo["id"]
        return store.list_findings(  # type: ignore[no-any-return]
            status=status,
            repo_id=repo_id,
            min_severity=severity,
        )

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


class _Server(ThreadingHTTPServer):
    allow_reuse_address = True
    allow_reuse_port = True


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


def daemon(cfg: Config) -> None:
    """Run forever: UI server + scheduler loop in one process.

    The loop shares _cycle_lock with POST /api/cycle, so manual and timed
    cycles never overlap. Idling costs zero tokens -- every wake goes through
    the budget gate, which is where all spending decisions live.

    Wake policy:
      - a job ran            -> 60s   (drain the queue quickly)
      - budget denied        -> until the 5h reset (+2min), capped at 30min
      - idle / no new work   -> 15min (upstream may push commits)
      - error                -> 5min
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

    from . import budget, scheduler
    from .store import Store

    while not stop.is_set():
        sleep_s: float = 15 * 60
        if _cycle_lock.acquire(blocking=False):
            try:
                store = Store(cfg)
                summary = scheduler.run_cycle(store, cfg)
                if "error" in summary:
                    sleep_s = 5 * 60
                elif summary.get("state") in (
                    "done",
                    "killed",
                    "failed",
                ):
                    sleep_s = 60
                elif summary.get("denied"):
                    w5 = budget.read_windows().get("anthropic:5h")
                    if w5 and w5.resets_at:
                        until = (w5.resets_at / 1000) - time.time() + 120
                        sleep_s = max(60.0, min(until, 30 * 60))
                    else:
                        sleep_s = 30 * 60
                log.info(
                    "cycle: %s -> sleep %ds",
                    json.dumps(summary)[:200],
                    sleep_s,
                )
            except Exception:
                log.exception("cycle crashed")
                sleep_s = 5 * 60
            finally:
                _cycle_lock.release()
        else:
            sleep_s = 60  # a UI-triggered cycle is running
        stop.wait(sleep_s)

    httpd.shutdown()
    httpd.server_close()
    log.info("daemon stopped")
