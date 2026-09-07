"""Tests for hunter.scheduler._refresh_stale_probe.

Root cause investigation (see the function's own docstring): headless
`omp -p` -- everything hunter's own workers use -- never refreshes
usage_history, confirmed empirically in production (identical recorded_at
before and after a real, token-spending `omp -p` run). This is the
proactive fix: force a fresh probe (`omp usage`, a cheap zero-inference
metadata call) whenever the current anthropic:5h reading is stale or
missing, gated on staleness rather than called every cycle since
Anthropic itself rate-limits /usage aggressively per source IP.
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pytest

from hunter import scheduler
from hunter.scheduler import _refresh_stale_probe
from hunter.types import Config, WindowState


def _cfg(**overrides: object) -> Config:
    defaults: dict[str, object] = {
        "work_root": Path("/tmp"),
        "db_path": Path("/tmp/test.db"),
        "stale_after_s": 1800,
        "omp_bin": "omp",
    }
    defaults.update(overrides)
    return Config(**defaults)  # type: ignore[arg-type]


def _ws(age_s: float) -> WindowState:
    now = time.time() * 1000
    return WindowState(
        limit_id="anthropic:5h",
        used_fraction=0.1,
        status="ok",
        resets_at=int(now + 3600_000),
        recorded_at=int(now - age_s * 1000),
        age_s=age_s,
    )


def _patched_run_cmd(monkeypatch: pytest.MonkeyPatch, rc: int = 0) -> list[list[str]]:
    calls: list[list[str]] = []

    def fake_run_cmd(cmd: list[str], timeout: int = 300) -> tuple[int, str]:
        calls.append(cmd)
        return rc, ""

    monkeypatch.setattr(scheduler, "run_cmd", fake_run_cmd)
    return calls


def test_no_windows_at_all_forces_a_probe(monkeypatch: pytest.MonkeyPatch) -> None:
    calls = _patched_run_cmd(monkeypatch)
    assert _refresh_stale_probe(_cfg(), {}) is True
    assert calls == [["omp", "usage"]]


def test_fresh_window_does_not_force_a_probe(monkeypatch: pytest.MonkeyPatch) -> None:
    calls = _patched_run_cmd(monkeypatch)
    windows = {"anthropic:5h": _ws(age_s=60.0)}
    assert _refresh_stale_probe(_cfg(stale_after_s=1800), windows) is False
    assert calls == []


def test_stale_window_forces_a_probe(monkeypatch: pytest.MonkeyPatch) -> None:
    calls = _patched_run_cmd(monkeypatch)
    windows = {"anthropic:5h": _ws(age_s=2000.0)}
    assert _refresh_stale_probe(_cfg(stale_after_s=1800), windows) is True
    assert calls == [["omp", "usage"]]


def test_exactly_at_threshold_does_not_force(monkeypatch: pytest.MonkeyPatch) -> None:
    """age_s == stale_after_s is still "fresh enough" -- only strictly
    older forces a probe, matching WindowState.stale's own > comparison
    style elsewhere in this codebase."""
    calls = _patched_run_cmd(monkeypatch)
    windows = {"anthropic:5h": _ws(age_s=1800.0)}
    assert _refresh_stale_probe(_cfg(stale_after_s=1800), windows) is False
    assert calls == []


def test_respects_configured_stale_after_s(monkeypatch: pytest.MonkeyPatch) -> None:
    """cfg.stale_after_s was loaded from config.json but never actually
    consulted anywhere before this fix -- prove it now has a real
    effect, not just a value sitting unused."""
    calls = _patched_run_cmd(monkeypatch)
    windows = {"anthropic:5h": _ws(age_s=500.0)}
    assert _refresh_stale_probe(_cfg(stale_after_s=1800), windows) is False
    assert _refresh_stale_probe(_cfg(stale_after_s=300), windows) is True
    assert calls == [["omp", "usage"]]


def test_uses_configured_omp_bin(monkeypatch: pytest.MonkeyPatch) -> None:
    calls = _patched_run_cmd(monkeypatch)
    _refresh_stale_probe(_cfg(omp_bin="/custom/path/omp"), {})
    assert calls == [["/custom/path/omp", "usage"]]


def test_failed_probe_returns_false(monkeypatch: pytest.MonkeyPatch) -> None:
    """run_cmd never raises (see util.run_cmd) -- a failed/timed-out
    probe just reports False, leaving windows exactly as stale as they
    already were. Callers already tolerate that via unaccounted_tokens."""
    _patched_run_cmd(monkeypatch, rc=1)
    assert _refresh_stale_probe(_cfg(), {}) is False
