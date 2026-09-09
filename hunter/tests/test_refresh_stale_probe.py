"""Tests for OmpScavengeBackend.keep_fresh().

Root cause investigation (see the function's own docstring): headless
`omp -p` -- everything hunter's own workers use -- never refreshes
usage_history, confirmed empirically in production (identical recorded_at
before and after a real, token-spending `omp -p` run). Separately,
plain `omp usage` alone was also confirmed insufficient: it has its own
internal cache, independent of hunter's own staleness gate, and can
silently return a cached report without reaching the network. Two
commands are required: `omp usage invalidate --provider anthropic`
(bust the cache) followed by `omp usage --provider anthropic` (a
genuinely fresh read), gated on staleness rather than called every
tick since Anthropic itself rate-limits /usage aggressively per source
IP.
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import TYPE_CHECKING
from unittest.mock import patch

if TYPE_CHECKING:
    import pytest

from hunter.backends.omp_scavenge.capacity import WindowState
from hunter.backends.omp_scavenge.facade import OmpScavengeBackend
from hunter.store import Store
from hunter.types import Config

_INVALIDATE = ["omp", "usage", "invalidate", "--provider", "anthropic"]
_READ = ["omp", "usage", "--provider", "anthropic"]


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


def _make_backend(cfg: Config, tmp_path: Path) -> OmpScavengeBackend:
    store_cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    store = Store(store_cfg)
    return OmpScavengeBackend(cfg=cfg, ledger=store)


def _patched_run_cmd_and_windows(
    monkeypatch: "pytest.MonkeyPatch",
    windows: dict[str, WindowState] | None = None,
    rc: int = 0,
) -> list[list[str]]:
    """Patch both run_cmd and read_windows for keep_fresh tests."""
    calls: list[list[str]] = []

    def fake_run_cmd(cmd: list[str], timeout: int = 300) -> tuple[int, str]:
        calls.append(cmd)
        return rc, ""

    monkeypatch.setattr("hunter.util.run_cmd", fake_run_cmd)
    monkeypatch.setattr(
        "hunter.backends.omp_scavenge.capacity.read_windows",
        lambda: windows if windows is not None else {},
    )
    return calls


def test_no_windows_at_all_forces_a_probe(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    cfg = _cfg()
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={})
    backend = _make_backend(cfg, tmp_path)
    assert backend.keep_fresh() is True
    assert calls == [_INVALIDATE, _READ]


def test_fresh_window_does_not_force_a_probe(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    cfg = _cfg(stale_after_s=1800)
    ws = _ws(age_s=60.0)
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={"anthropic:5h": ws})
    backend = _make_backend(cfg, tmp_path)
    assert backend.keep_fresh() is False
    assert calls == []


def test_stale_window_forces_a_probe(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    cfg = _cfg(stale_after_s=1800)
    ws = _ws(age_s=2000.0)
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={"anthropic:5h": ws})
    backend = _make_backend(cfg, tmp_path)
    assert backend.keep_fresh() is True
    assert calls == [_INVALIDATE, _READ]


def test_exactly_at_threshold_does_not_force(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    """age_s == stale_after_s is still "fresh enough" -- only strictly
    older forces a probe, matching WindowState.stale's own > comparison
    style elsewhere in this codebase."""
    cfg = _cfg(stale_after_s=1800)
    ws = _ws(age_s=1800.0)
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={"anthropic:5h": ws})
    backend = _make_backend(cfg, tmp_path)
    assert backend.keep_fresh() is False
    assert calls == []


def test_respects_configured_stale_after_s(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    """cfg.stale_after_s was loaded from config.json but never actually
    consulted anywhere before this fix -- prove it now has a real
    effect, not just a value sitting unused."""
    ws = _ws(age_s=500.0)
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={"anthropic:5h": ws})

    backend_fresh = _make_backend(_cfg(stale_after_s=1800), tmp_path)
    assert backend_fresh.keep_fresh() is False

    backend_stale = _make_backend(_cfg(stale_after_s=300), tmp_path)
    assert backend_stale.keep_fresh() is True
    assert calls == [_INVALIDATE, _READ]


def test_invalidates_before_reading_so_the_read_cannot_serve_a_stale_cache(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    """Ordering matters: invalidate must run BEFORE the read, or the
    read could serve omp's own internal cache instead of a fresh
    fetch -- exactly the failure mode observed live (a plain `omp
    usage` reported "fetched 32.6s ago" while hunter's own DB row was
    over a minute staler than that)."""
    cfg = _cfg()
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={})
    backend = _make_backend(cfg, tmp_path)
    backend.keep_fresh()
    assert calls[0] == _INVALIDATE
    assert calls[1] == _READ


def test_uses_configured_omp_bin(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    cfg = _cfg(omp_bin="/custom/path/omp")
    calls = _patched_run_cmd_and_windows(monkeypatch, windows={})
    backend = _make_backend(cfg, tmp_path)
    backend.keep_fresh()
    assert calls == [
        ["/custom/path/omp", "usage", "invalidate", "--provider", "anthropic"],
        ["/custom/path/omp", "usage", "--provider", "anthropic"],
    ]


def test_failed_probe_returns_false(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    """run_cmd never raises (see util.run_cmd) -- a failed/timed-out
    probe just reports False, leaving windows exactly as stale as they
    already were. Callers already tolerate that via unaccounted_tokens."""
    cfg = _cfg()
    _patched_run_cmd_and_windows(monkeypatch, windows={}, rc=1)
    backend = _make_backend(cfg, tmp_path)
    assert backend.keep_fresh() is False


def test_failed_invalidate_does_not_block_the_read_attempt(
    monkeypatch: "pytest.MonkeyPatch", tmp_path: Path,
) -> None:
    """The invalidate step is best-effort like everything else here --
    if it fails, still attempt the read (which may just serve a cache
    in that fallback case, but that's strictly no worse than not
    trying at all)."""
    calls: list[list[str]] = []

    def fake_run_cmd(cmd: list[str], timeout: int = 300) -> tuple[int, str]:
        calls.append(cmd)
        return (1, "") if cmd == _INVALIDATE else (0, "")

    monkeypatch.setattr("hunter.util.run_cmd", fake_run_cmd)
    monkeypatch.setattr(
        "hunter.backends.omp_scavenge.capacity.read_windows", lambda: {},
    )
    cfg = _cfg()
    backend = _make_backend(cfg, tmp_path)
    assert backend.keep_fresh() is True
    assert calls == [_INVALIDATE, _READ]
