"""Proof that setting a budget override ('once' or 'exempt') wakes the
daemon loop immediately, via a real HTTP request against a real server --
not a mock of the handler. This is the "tighter loop" mechanism for
exempted findings: POST /api/override sets _wake, which the daemon's
sleep loop polls and breaks out of early regardless of how long the
current budget-denied backoff was going to be.
"""

from __future__ import annotations

import http.client
import json
import threading
import time
from pathlib import Path
from typing import Any

import pytest

from hunter.server import _wake, make_server
from hunter.store import Store
from hunter.types import Config


def _make_finding(**overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "fingerprint": "repo:f.py:fn:logic",
        "file": "f.py",
        "symbol": "fn",
        "line": 10,
        "bug_class": "logic",
        "severity": "high",
        "confidence": 0.9,
        "summary": "Bug found",
        "detail": "Details here",
        "evidence_plan": "plan",
        "introduced_by": "abc123",
    }
    base.update(overrides)
    return base


class TestOverrideWakesLoop:
    def setup_method(self) -> None:
        _wake.clear()

    def teardown_method(self) -> None:
        _wake.clear()

    def _wait_for_wake(self, timeout: float = 1.0) -> bool:
        """Poll _wake rather than asserting the instant an HTTP response
        is received: the response completing only guarantees the server
        determined its result, not that every statement after
        self._json(...) in the handler has executed yet."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if _wake.is_set():
                return True
            time.sleep(0.01)
        return _wake.is_set()

    def _post(self, port: int, path: str, body: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        try:
            conn.request(
                "POST", path, json.dumps(body), {"Content-Type": "application/json"}
            )
            resp = conn.getresponse()
            raw = resp.read()
            return resp.status, json.loads(raw) if raw else {}
        finally:
            conn.close()

    def test_setting_exempt_override_wakes_the_daemon_loop(self, tmp_path: Path) -> None:
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
        cfg.serve_port = 0  # OS-assigned free port
        store = Store(cfg)
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "queued")

        httpd = make_server(cfg)
        port = httpd.server_address[1]
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        try:
            assert not _wake.is_set()
            status, body = self._post(port, "/api/override", {"id": fid, "mode": "exempt"})
            assert status == 200, body
            assert self._wait_for_wake(), (
                "_wake must be set immediately when a budget override is set -- "
                "this is what gives an exempted finding a tighter loop instead "
                "of waiting out whatever backoff the daemon last computed"
            )
        finally:
            httpd.shutdown()
            httpd.server_close()

    def test_setting_once_override_also_wakes(self, tmp_path: Path) -> None:
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
        cfg.serve_port = 0
        store = Store(cfg)
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "queued")

        httpd = make_server(cfg)
        port = httpd.server_address[1]
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        try:
            status, body = self._post(port, "/api/override", {"id": fid, "mode": "once"})
            assert status == 200, body
            assert self._wait_for_wake()
        finally:
            httpd.shutdown()
            httpd.server_close()

    def test_clearing_override_does_not_wake(self, tmp_path: Path) -> None:
        """Clearing an override (mode=null) is not urgent -- no reason to
        interrupt whatever backoff is already in progress."""
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
        cfg.serve_port = 0
        store = Store(cfg)
        rid = store.add_repo("r", "https://r", "/r")
        fid, _ = store.upsert_finding(rid, _make_finding())
        store.set_status(fid, "queued")
        store.set_budget_override(fid, "exempt")

        httpd = make_server(cfg)
        port = httpd.server_address[1]
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        try:
            assert not _wake.is_set()
            status, body = self._post(port, "/api/override", {"id": fid, "mode": None})
            assert status == 200, body
            time.sleep(0.2)  # grace period -- confirm it stays unset, not "checked too early"
            assert not _wake.is_set()
        finally:
            httpd.shutdown()
            httpd.server_close()
