"""Shared types + config for the Idle-Token Bug Hunter.

Everything is stdlib. Config lives in hunter/config.json next to the package
dir; paths in config are resolved relative to the project root (the directory
containing the hunter/ package tree).
"""
from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent  # .../hunter
SCHEMA_PATH = PROJECT_ROOT / "schema.sql"
PLAYBOOK_DIR = PROJECT_ROOT / "playbooks"
UI_DIR = PROJECT_ROOT / "ui"
OMP_SESSIONS_DIR = Path.home() / ".omp/agent/sessions"
OMP_AGENT_DB = Path.home() / ".omp/agent/agent.db"

FINDING_STATUSES = (
    "new", "queued", "fixing", "pr_open", "merged", "rejected", "wontfix", "note",
)
# Verdicts that feed the suppression corpus (hunt prompt "known non-bugs")
SUPPRESSED_STATUSES = ("rejected", "wontfix")
# Statuses that block re-hunting the same fingerprint entirely
ACTIVE_STATUSES = ("new", "queued", "fixing", "pr_open", "merged", "note")

BUG_CLASSES = ("boundary", "error-path", "race", "contract-drift", "leak", "logic")


def now_ms() -> int:
    return int(time.time() * 1000)


@dataclass
class Config:
    work_root: Path
    db_path: Path
    omp_bin: str = "omp"
    hunt_cap_tokens: int = 200_000
    hunt_max_wall_s: int = 1800
    hunt_max_findings: int = 8
    fix_cap_tokens: int = 150_000
    fix_max_wall_s: int = 2700
    interactive_reserve_7d: float = 0.25
    deny_5h_above: float = 0.85
    stale_after_s: int = 1800
    serve_port: int = 8377
    poll_s: float = 2.0
    model_default: str | None = None    # --model for all workers (None = omp default)
    model_smol: str | None = None       # --smol helper model for lightweight subtasks
    model_hunt: str | None = None       # per-kind overrides of model_default
    model_fix: str | None = None

    def model_for(self, kind: str) -> str | None:
        override = self.model_hunt if kind == "hunt" else self.model_fix
        return override or self.model_default

    @staticmethod
    def load(path: Path | None = None) -> "Config":
        p = path or (PROJECT_ROOT / "config.json")
        raw = json.loads(p.read_text()) if p.exists() else {}
        root = PROJECT_ROOT

        def rp(v: str) -> Path:
            q = Path(v)
            return q if q.is_absolute() else (root / q).resolve()

        return Config(
            work_root=rp(raw.get("workRoot", "data")),
            db_path=rp(raw.get("dbPath", "data/hunter.db")),
            omp_bin=raw.get("ompBin", "omp"),
            hunt_cap_tokens=raw.get("hunt", {}).get("capNewTokens", 200_000),
            hunt_max_wall_s=raw.get("hunt", {}).get("maxWallS", 1800),
            hunt_max_findings=raw.get("hunt", {}).get("maxFindings", 8),
            fix_cap_tokens=raw.get("fix", {}).get("capNewTokens", 150_000),
            fix_max_wall_s=raw.get("fix", {}).get("maxWallS", 2700),
            interactive_reserve_7d=raw.get("budget", {}).get("interactiveReserve7d", 0.25),
            deny_5h_above=raw.get("budget", {}).get("deny5hAbove", 0.85),
            stale_after_s=raw.get("budget", {}).get("staleAfterS", 1800),
            serve_port=raw.get("serve", {}).get("port", 8377),
            poll_s=raw.get("pollS", 2.0),
            model_default=raw.get("models", {}).get("default"),
            model_smol=raw.get("models", {}).get("smol"),
            model_hunt=raw.get("models", {}).get("hunt"),
            model_fix=raw.get("models", {}).get("fix"),
        )


@dataclass
class RunResult:
    """Outcome of one worker run (see runner.run_worker)."""
    exit_code: int | None
    killed_reason: str | None       # None | "cap" | "wallclock"
    tokens_new: int                 # input + output + cacheWrite from the ledger
    calls: int
    session_file: str | None        # the worker's JSONL, for post-mortems
    duration_s: float
    stdout_tail: str = ""


@dataclass
class WindowState:
    limit_id: str
    used_fraction: float | None
    status: str | None              # ok | exhausted | ...
    resets_at: int | None           # epoch ms
    recorded_at: int                # epoch ms — when omp probed it
    age_s: float = field(default=0.0)

    @property
    def stale(self) -> bool:
        return self.age_s > 1800


@dataclass
class BudgetDecision:
    allow: bool
    reason: str
    cap_tokens: int = 0             # effective per-job cap when allowed
