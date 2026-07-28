"""Triage UI server for the Idle-Token Bug Hunter.

ThreadingHTTPServer + hand-rolled JSON routes; serves ui/index.html.
Thread safety: a fresh Store (own sqlite connection) per request.
Heavy modules (store, budget, scheduler) are imported lazily inside
handlers so the server module stays importable while siblings build.
"""
from __future__ import annotations

import json
import threading
import traceback
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from .types import FINDING_STATUSES, VERDICT_STATUSES, REASON_REQUIRED, Config, UI_DIR

# One cycle at a time, across all request threads.
_cycle_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    cfg: Config  # set by make_server()
    server_version = "hunter/1"
    protocol_version = "HTTP/1.1"

    # -- plumbing ---------------------------------------------------------

    def log_message(self, fmt, *args):  # keep stdout for the operator
        pass

    def _store(self):
        from .store import Store
        return Store(self.cfg)

    def _send(self, status: int, body: bytes, ctype: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, obj, status: int = 200) -> None:
        self._send(status, json.dumps(obj).encode(), "application/json")

    def _error(self, status: int, message: str) -> None:
        self._json({"error": message}, status)

    def _body_json(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if not raw:
            raise ValueError("empty body")
        obj = json.loads(raw)
        if not isinstance(obj, dict):
            raise ValueError("body must be a JSON object")
        return obj

    # -- GET --------------------------------------------------------------

    def do_GET(self):
        url = urlparse(self.path)
        qs = parse_qs(url.query)
        try:
            if url.path in ("/", "/index.html"):
                page = UI_DIR / "index.html"
                if not page.is_file():
                    return self._error(404, "ui/index.html missing")
                return self._send(200, page.read_bytes(), "text/html; charset=utf-8")
            if url.path == "/api/summary":
                return self._json(self._summary())
            if url.path == "/api/findings":
                return self._json(self._findings(qs))
            if url.path == "/api/jobs":
                return self._json(self._store().list_jobs(limit=50))
            if url.path == "/api/repos":
                return self._json(self._store().list_repos())
            if url.path == "/api/events":
                return self._json(self._store().recent_events(limit=100))
            return self._error(404, "not found")
        except Exception as exc:  # surface, don't kill the thread
            traceback.print_exc()
            self._error(500, f"{type(exc).__name__}: {exc}")

    def _summary(self) -> dict:
        from . import budget
        windows = {}
        for limit_id, w in budget.read_windows().items():
            windows[limit_id] = {
                "used_fraction": w.used_fraction,
                "status": w.status,
                "resets_at": w.resets_at,
                "age_s": w.age_s,
                "stale": w.stale,
            }
        store = self._store()
        counts = {s: 0 for s in FINDING_STATUSES}
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

    def _findings(self, qs: dict) -> list[dict]:
        store = self._store()
        status = (qs.get("status") or [None])[0] or None
        repo_key = (qs.get("repo") or [None])[0] or None
        repo_id = None
        if repo_key is not None:
            key = int(repo_key) if repo_key.isdigit() else repo_key
            repo = store.get_repo(key)
            if repo is None:
                raise ValueError(f"unknown repo {repo_key!r}")
            repo_id = repo["id"]
        return store.list_findings(status=status, repo_id=repo_id)

    # -- POST -------------------------------------------------------------

    def do_POST(self):
        url = urlparse(self.path)
        try:
            if url.path == "/api/verdict":
                return self._verdict()
            if url.path == "/api/cycle":
                return self._cycle()
            if url.path == "/api/recheck":
                return self._recheck()
            return self._error(404, "not found")
        except (ValueError, json.JSONDecodeError) as exc:
            self._error(400, str(exc))
        except Exception as exc:
            traceback.print_exc()
            self._error(500, f"{type(exc).__name__}: {exc}")

    def _verdict(self) -> None:
        body = self._body_json()
        fid = body.get("id")
        status = body.get("status")
        reason = (body.get("reason") or "").strip() or None
        if not isinstance(fid, int):
            return self._error(400, "id must be an integer")
        if status not in VERDICT_STATUSES:
            return self._error(400, f"status must be one of {list(VERDICT_STATUSES)}")
        if status in REASON_REQUIRED and not reason:
            return self._error(400, f"reason required for status {status!r}")
        store = self._store()
        finding = store.get_finding(fid)
        if finding is None:
            return self._error(404, f"no finding {fid}")
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
            return self._json({"error": "busy"}, 409)
        cfg = self.cfg

        def run():
            try:
                from . import scheduler
                from .store import Store
                store = Store(cfg)
                try:
                    scheduler.run_cycle(store, cfg)
                except Exception as exc:
                    traceback.print_exc()
                    try:
                        store.log_event("error", f"cycle failed: {exc}")
                    except Exception:
                        pass
            finally:
                _cycle_lock.release()

        threading.Thread(target=run, name="hunter-cycle", daemon=True).start()
        self._json({"started": True}, 202)

    def _recheck(self) -> None:
        body = self._body_json()
        fid = body.get("id")
        if not isinstance(fid, int):
            return self._error(400, "id must be an integer")
        store = self._store()
        finding = store.get_finding(fid)
        if finding is None:
            return self._error(404, f"no finding {fid}")
        if finding["status"] not in ("new",):
            return self._error(400, f"finding #{fid} is {finding['status']!r}, not 'new'")
        store.set_status(fid, "rechecking")
        store.log_event("recheck", f"#{fid} queued for recheck", finding_id=fid)
        self._json({"queued": True, "finding": store.get_finding(fid)})


def make_server(cfg: Config, port: int | None = None) -> ThreadingHTTPServer:
    Handler.cfg = cfg
    httpd = ThreadingHTTPServer(("127.0.0.1", port or cfg.serve_port), Handler)
    httpd.daemon_threads = True
    return httpd


def serve(cfg: Config) -> None:
    httpd = make_server(cfg)
    print(f"hunter ui: http://127.0.0.1:{httpd.server_address[1]}/")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        httpd.server_close()


def daemon(cfg: Config) -> None:
    """Run forever: UI server + scheduler loop in one process.

    The loop shares _cycle_lock with POST /api/cycle, so manual and timed
    cycles never overlap. Idling costs zero tokens — every wake goes through
    the budget gate, which is where all spending decisions live.

    Wake policy:
      - a job ran            -> 60s   (drain the queue quickly)
      - budget denied        -> until the 5h reset (+2min), capped at 30min
      - idle / no new work   -> 15min (upstream may push commits)
      - error                -> 5min
    """
    import signal as _signal
    import time as _time

    httpd = make_server(cfg)
    threading.Thread(target=httpd.serve_forever, name="hunter-ui", daemon=True).start()
    print(f"hunter daemon: ui http://127.0.0.1:{httpd.server_address[1]}/ — scheduler loop live", flush=True)

    stop = threading.Event()
    for sig in (_signal.SIGTERM, _signal.SIGINT):
        _signal.signal(sig, lambda *_: stop.set())

    from . import budget, scheduler
    from .store import Store

    while not stop.is_set():
        sleep_s = 15 * 60
        if _cycle_lock.acquire(blocking=False):
            try:
                store = Store(cfg)
                summary = scheduler.run_cycle(store, cfg)
                if "error" in summary:
                    sleep_s = 5 * 60
                elif summary.get("state") in ("done", "killed", "failed"):
                    sleep_s = 60
                elif summary.get("denied"):
                    w5 = budget.read_windows().get("anthropic:5h")
                    if w5 and w5.resets_at:
                        until = (w5.resets_at / 1000) - _time.time() + 120
                        sleep_s = max(60, min(until, 30 * 60))
                    else:
                        sleep_s = 30 * 60
                print(f"cycle: {json.dumps(summary)[:200]} -> sleep {sleep_s:.0f}s", flush=True)
            except Exception as exc:
                traceback.print_exc()
                sleep_s = 5 * 60
            finally:
                _cycle_lock.release()
        else:
            sleep_s = 60  # a UI-triggered cycle is running
        stop.wait(sleep_s)

    httpd.shutdown()
    httpd.server_close()
    print("hunter daemon: stopped", flush=True)
