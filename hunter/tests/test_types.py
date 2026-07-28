"""Tests for hunter.types module."""

from __future__ import annotations

import json
import time
from enum import StrEnum

import pytest

from hunter.types import (
    ACTIVE_STATUSES,
    FINDING_STATUSES,
    REASON_REQUIRED,
    SUPPRESSED_STATUSES,
    VERDICT_STATUSES,
    BudgetDecision,
    BugClass,
    Config,
    RunResult,
    Status,
    WindowState,
    now_ms,
)

# ── Status enum ──────────────────────────────────────────────────────


class TestStatus:
    def test_is_str_enum(self):
        assert issubclass(Status, StrEnum)

    @pytest.mark.parametrize(
        ("member", "value"),
        [
            ("NEW", "new"),
            ("RECHECKING", "rechecking"),
            ("QUEUED", "queued"),
            ("FIXING", "fixing"),
            ("PR_OPEN", "pr_open"),
            ("MERGED", "merged"),
            ("REJECTED", "rejected"),
            ("WONTFIX", "wontfix"),
            ("NOTE", "note"),
        ],
    )
    def test_members(self, member, value):
        assert Status[member].value == value
        assert str(Status[member]) == value

    def test_member_count(self):
        assert len(Status) == 9


# ── BugClass enum ────────────────────────────────────────────────────


class TestBugClass:
    def test_is_str_enum(self):
        assert issubclass(BugClass, StrEnum)

    @pytest.mark.parametrize(
        ("member", "value"),
        [
            ("BOUNDARY", "boundary"),
            ("ERROR_PATH", "error-path"),
            ("RACE", "race"),
            ("CONTRACT_DRIFT", "contract-drift"),
            ("LEAK", "leak"),
            ("LOGIC", "logic"),
        ],
    )
    def test_members(self, member, value):
        assert BugClass[member].value == value

    def test_member_count(self):
        assert len(BugClass) == 6


# ── Status tuples ────────────────────────────────────────────────────


class TestStatusTuples:
    def test_finding_statuses_is_all(self):
        assert set(FINDING_STATUSES) == set(Status)

    def test_suppressed_statuses(self):
        assert set(SUPPRESSED_STATUSES) == {Status.REJECTED, Status.WONTFIX}

    def test_active_statuses(self):
        expected = {
            Status.NEW,
            Status.RECHECKING,
            Status.QUEUED,
            Status.FIXING,
            Status.PR_OPEN,
            Status.MERGED,
            Status.NOTE,
        }
        assert set(ACTIVE_STATUSES) == expected

    def test_active_excludes_suppressed(self):
        assert not set(ACTIVE_STATUSES) & set(SUPPRESSED_STATUSES)

    def test_verdict_statuses(self):
        expected = {
            Status.QUEUED,
            Status.REJECTED,
            Status.WONTFIX,
            Status.NOTE,
            Status.MERGED,
        }
        assert set(VERDICT_STATUSES) == expected

    def test_reason_required(self):
        assert set(REASON_REQUIRED) == {Status.REJECTED, Status.WONTFIX}


# ── now_ms ───────────────────────────────────────────────────────────


class TestNowMs:
    def test_returns_int(self):
        assert isinstance(now_ms(), int)

    def test_monotonic(self):
        a = now_ms()
        time.sleep(0.002)
        b = now_ms()
        assert b >= a

    def test_plausible_epoch_ms(self):
        # Should be a 13-digit unix timestamp in ms
        v = now_ms()
        assert v > 1_700_000_000_000  # after 2023-11


# ── Config ───────────────────────────────────────────────────────────


class TestConfig:
    def test_defaults(self, tmp_path):
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "h.db")
        assert cfg.omp_bin == "omp"
        assert cfg.hunt_cap_tokens == 200_000
        assert cfg.hunt_max_wall_s == 1800
        assert cfg.hunt_max_findings == 8
        assert cfg.fix_cap_tokens == 150_000
        assert cfg.fix_max_wall_s == 2700
        assert cfg.deny_5h_above == 0.85
        assert cfg.stale_after_s == 1800
        assert cfg.serve_port == 8377
        assert cfg.poll_s == 2.0
        assert cfg.model_default is None
        assert cfg.model_smol is None
        assert cfg.model_hunt is None
        assert cfg.model_fix is None

    def test_model_for_hunt_override(self, tmp_path):
        cfg = Config(
            work_root=tmp_path,
            db_path=tmp_path / "h.db",
            model_default="default-m",
            model_hunt="hunt-m",
        )
        assert cfg.model_for("hunt") == "hunt-m"

    def test_model_for_fix_override(self, tmp_path):
        cfg = Config(
            work_root=tmp_path,
            db_path=tmp_path / "h.db",
            model_default="default-m",
            model_fix="fix-m",
        )
        assert cfg.model_for("fix") == "fix-m"

    def test_model_for_falls_back_to_default(self, tmp_path):
        cfg = Config(
            work_root=tmp_path,
            db_path=tmp_path / "h.db",
            model_default="default-m",
        )
        assert cfg.model_for("hunt") == "default-m"
        assert cfg.model_for("fix") == "default-m"

    def test_model_for_none_when_no_override_or_default(self, tmp_path):
        cfg = Config(work_root=tmp_path, db_path=tmp_path / "h.db")
        assert cfg.model_for("hunt") is None
        assert cfg.model_for("fix") is None

    def test_load_from_json(self, tmp_path):
        config_data = {
            "workRoot": str(tmp_path / "wr"),
            "dbPath": str(tmp_path / "my.db"),
            "ompBin": "/usr/bin/omp",
            "hunt": {"capNewTokens": 100_000, "maxWallS": 600, "maxFindings": 3},
            "fix": {"capNewTokens": 50_000, "maxWallS": 900},
            "budget": {
                "deny5hAbove": 0.90,
                "staleAfterS": 900,
            },
            "serve": {"port": 9999},
            "pollS": 5.0,
            "models": {
                "default": "claude-sonnet",
                "smol": "claude-haiku",
                "hunt": "claude-opus",
                "fix": "claude-sonnet",
            },
        }
        cfg_path = tmp_path / "config.json"
        cfg_path.write_text(json.dumps(config_data))

        cfg = Config.load(cfg_path)
        assert cfg.work_root == tmp_path / "wr"
        assert cfg.db_path == tmp_path / "my.db"
        assert cfg.omp_bin == "/usr/bin/omp"
        assert cfg.hunt_cap_tokens == 100_000
        assert cfg.hunt_max_wall_s == 600
        assert cfg.hunt_max_findings == 3
        assert cfg.fix_cap_tokens == 50_000
        assert cfg.fix_max_wall_s == 900
        assert cfg.deny_5h_above == 0.90
        assert cfg.stale_after_s == 900
        assert cfg.serve_port == 9999
        assert cfg.poll_s == 5.0
        assert cfg.model_default == "claude-sonnet"
        assert cfg.model_smol == "claude-haiku"
        assert cfg.model_hunt == "claude-opus"
        assert cfg.model_fix == "claude-sonnet"

    def test_load_missing_file_uses_defaults(self, tmp_path):
        cfg = Config.load(tmp_path / "nonexistent.json")
        assert cfg.hunt_cap_tokens == 200_000
        assert cfg.serve_port == 8377
        assert cfg.model_default is None

    def test_load_relative_paths_resolved(self, tmp_path):
        # When workRoot is relative, it resolves against PROJECT_ROOT
        cfg_path = tmp_path / "config.json"
        cfg_path.write_text(json.dumps({"workRoot": "data"}))
        cfg = Config.load(cfg_path)
        assert cfg.work_root.is_absolute()


# ── RunResult ────────────────────────────────────────────────────────


class TestRunResult:
    def test_construction(self):
        rr = RunResult(
            exit_code=0,
            killed_reason=None,
            tokens_new=5000,
            calls=10,
            session_file="/tmp/s.jsonl",
            duration_s=42.5,
        )
        assert rr.exit_code == 0
        assert rr.killed_reason is None
        assert rr.tokens_new == 5000
        assert rr.calls == 10
        assert rr.session_file == "/tmp/s.jsonl"
        assert rr.duration_s == 42.5
        assert rr.stdout_tail == ""

    def test_construction_killed(self):
        rr = RunResult(
            exit_code=None,
            killed_reason="cap",
            tokens_new=200_000,
            calls=50,
            session_file=None,
            duration_s=1800.0,
            stdout_tail="last output",
        )
        assert rr.exit_code is None
        assert rr.killed_reason == "cap"
        assert rr.stdout_tail == "last output"


# ── WindowState ──────────────────────────────────────────────────────


class TestWindowState:
    def test_construction(self):
        ws = WindowState(
            limit_id="anthropic:5h",
            used_fraction=0.5,
            status="ok",
            resets_at=1700000000000,
            recorded_at=1700000000000,
            age_s=100.0,
        )
        assert ws.limit_id == "anthropic:5h"
        assert ws.used_fraction == 0.5
        assert ws.status == "ok"

    def test_stale_at_boundary(self):
        ws_exact = WindowState(
            limit_id="x",
            used_fraction=0.0,
            status="ok",
            resets_at=0,
            recorded_at=0,
            age_s=1800.0,
        )
        assert ws_exact.stale is False  # exactly 1800 is NOT stale (> not >=)

    def test_stale_above_boundary(self):
        ws_over = WindowState(
            limit_id="x",
            used_fraction=0.0,
            status="ok",
            resets_at=0,
            recorded_at=0,
            age_s=1800.1,
        )
        assert ws_over.stale is True

    def test_not_stale_below_boundary(self):
        ws_under = WindowState(
            limit_id="x",
            used_fraction=0.0,
            status="ok",
            resets_at=0,
            recorded_at=0,
            age_s=1799.9,
        )
        assert ws_under.stale is False

    def test_default_age(self):
        ws = WindowState(
            limit_id="x",
            used_fraction=None,
            status=None,
            resets_at=None,
            recorded_at=0,
        )
        assert ws.age_s == 0.0
        assert ws.stale is False


# ── BudgetDecision ───────────────────────────────────────────────────


class TestBudgetDecision:
    def test_allow(self):
        bd = BudgetDecision(allow=True, reason="go", cap_tokens=100_000)
        assert bd.allow is True
        assert bd.reason == "go"
        assert bd.cap_tokens == 100_000

    def test_deny(self):
        bd = BudgetDecision(allow=False, reason="over budget")
        assert bd.allow is False
        assert bd.cap_tokens == 0
