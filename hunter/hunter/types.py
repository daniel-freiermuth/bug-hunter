"""Shared types + config for the Idle-Token Bug Hunter.

Everything is stdlib. Config lives in hunter/config.json next to the package
dir; paths in config are resolved relative to the project root (the directory
containing the hunter/ package tree).
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from enum import IntEnum, StrEnum
from pathlib import Path
from typing import Any, NotRequired, TypedDict

PROJECT_ROOT = Path(__file__).resolve().parent.parent  # .../hunter
SCHEMA_PATH = PROJECT_ROOT / "schema.sql"
PLAYBOOK_DIR = PROJECT_ROOT / "playbooks"
UI_DIR = PROJECT_ROOT / "ui"
OMP_SESSIONS_DIR = Path.home() / ".omp/agent/sessions"
OMP_AGENT_DB = Path.home() / ".omp/agent/agent.db"


class Status(StrEnum):
    NEW = "new"
    RECHECKING = "rechecking"
    QUEUED = "queued"
    FIXING = "fixing"
    PR_OPEN = "pr_open"
    MERGED = "merged"
    REJECTED = "rejected"
    WONTFIX = "wontfix"
    NOTE = "note"


class BugClass(StrEnum):
    BOUNDARY = "boundary"
    ERROR_PATH = "error-path"
    RACE = "race"
    CONTRACT_DRIFT = "contract-drift"
    LEAK = "leak"
    LOGIC = "logic"


class Severity(IntEnum):
    """Ordered severity — higher value = more severe."""

    LOW = 1
    MEDIUM = 2
    HIGH = 3

    @classmethod
    def from_str(cls, s: str) -> Severity:
        """Parse a severity string (case-insensitive)."""
        try:
            return cls[s.upper()]
        except KeyError:
            msg = f"unknown severity {s!r} (expected {', '.join(m.name.lower() for m in cls)})"
            raise ValueError(msg) from None

    @classmethod
    def at_or_above(cls, minimum: Severity) -> tuple[str, ...]:
        """Severity string values at or above *minimum*."""
        return tuple(m.name.lower() for m in cls if m >= minimum)


FINDING_STATUSES: tuple[Status, ...] = tuple(Status)
SUPPRESSED_STATUSES: tuple[Status, ...] = (Status.REJECTED, Status.WONTFIX)
ACTIVE_STATUSES: tuple[Status, ...] = (
    Status.NEW,
    Status.RECHECKING,
    Status.QUEUED,
    Status.FIXING,
    Status.PR_OPEN,
    Status.MERGED,
    Status.NOTE,
)
# Human verdicts allowed from the UI/CLI
VERDICT_STATUSES: tuple[Status, ...] = (
    Status.QUEUED,
    Status.REJECTED,
    Status.WONTFIX,
    Status.NOTE,
    Status.MERGED,
)
REASON_REQUIRED: tuple[Status, ...] = (Status.REJECTED, Status.WONTFIX)
BUG_CLASSES: tuple[BugClass, ...] = tuple(BugClass)
SEVERITIES: tuple[str, ...] = tuple(m.name.lower() for m in Severity)


def now_ms() -> int:
    return int(time.time() * 1000)


# Type alias for rows returned from SQLite (dict with str keys).
Row = dict[str, Any]


# Precise shapes for the two SQL-backed rows that cross the /api/summary
# HTTP boundary (see server.py's _activity_status). Everything else in
# this codebase still uses the loose Row alias above -- these two exist
# specifically because untyped access to them (scheduler_state["detail"])
# produced a real, demonstrated bug: mypy --strict cannot catch a missing
# or misspelled key on dict[str, Any], by design, no matter how strict
# the rest of the config is. Store.get_scheduler_state()/current_job()
# verify these shapes at runtime (the one place a SQL/schema drift could
# actually violate them) before asserting the type -- see
# store._require_keys. Everything downstream of that one checked
# boundary gets full mypy coverage instead of Any all the way through.
class SchedulerStateDict(TypedDict):
    id: int
    state: str
    detail: str
    next_wake_at: int | None
    updated_at: int


class JobDict(TypedDict):
    id: int
    kind: str
    repo_id: int
    repo_name: str
    finding_id: int | None
    state: str
    pid: int | None
    session_file: str | None
    cap_tokens: int | None
    tokens_new: int | None
    calls: int | None
    exit_code: int | None
    killed_reason: str | None
    notes: str | None
    started_at: int | None
    finished_at: int | None
    model: str | None
    usage_delta: float | None
    # Only present when finding_id is set (current_job() looks the
    # finding up separately) -- NotRequired, not "| None", because the
    # key is genuinely absent rather than present-with-null in that case.
    finding_summary: NotRequired[str | None]
    finding_fingerprint: NotRequired[str | None]


class RepoDict(TypedDict):
    """SELECT * FROM repos -- see schema.sql. Store.list_repos()/get_repo()
    stay Row-typed (their other callers need dict[str, Any]); this exists
    for the /api/summary boundary specifically, where server.py casts to
    it and SummaryDict's pydantic validation re-verifies it at runtime."""

    id: int
    name: str
    url: str
    path: str
    forge: str
    default_branch: str
    last_hunt_sha: str | None
    last_hunt_at: int | None
    enabled: int
    added_at: int


class EventDict(TypedDict):
    """SELECT * FROM events -- see schema.sql."""

    id: int
    at: int
    kind: str
    message: str
    job_id: int | None
    finding_id: int | None


@dataclass
class Config:
    work_root: Path
    db_path: Path
    omp_bin: str = "omp"
    hunt_cap_tokens: int = 200_000
    hunt_max_wall_s: int = 1800
    hunt_max_findings: int = 8
    hunt_rehunt_days: int = 90  # Full re-hunt interval
    modernization_interval_days: int = 30  # min days between modernization scans per repo
    fix_cap_tokens: int = 150_000
    fix_max_wall_s: int = 2700
    stale_after_s: int = 1800
    serve_port: int = 8377
    poll_s: float = 2.0
    model_default: str | None = None  # --model for all workers (None = omp default)
    model_smol: str | None = None  # --smol helper model for lightweight subtasks
    model_hunt: str | None = None  # per-kind overrides of model_default
    model_fix: str | None = None

    def model_for(self, kind: str) -> str | None:
        override = self.model_hunt if kind == "hunt" else self.model_fix
        return override or self.model_default

    @staticmethod
    def load(path: Path | None = None) -> Config:
        p = path or (PROJECT_ROOT / "config.json")
        raw: dict[str, Any] = json.loads(p.read_text()) if p.exists() else {}
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
            hunt_rehunt_days=raw.get("hunt", {}).get("rehuntDays", 90),
            modernization_interval_days=raw.get("modernization", {}).get("intervalDays", 30),
            fix_cap_tokens=raw.get("fix", {}).get("capNewTokens", 150_000),
            fix_max_wall_s=raw.get("fix", {}).get("maxWallS", 2700),
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
    killed_reason: str | None  # None | "cap" | "wallclock"
    tokens_new: int  # input + output + cacheWrite from the ledger
    calls: int
    session_file: str | None  # the worker's JSONL, for post-mortems
    duration_s: float
    stdout_tail: str = ""


@dataclass
class WindowState:
    limit_id: str
    used_fraction: float | None
    status: str | None  # ok | exhausted | ...
    resets_at: int | None  # epoch ms
    recorded_at: int  # epoch ms -- when omp probed it
    age_s: float = field(default=0.0)

    @property
    def stale(self) -> bool:
        return self.age_s > 1800


@dataclass
class UnaccountedTokens:
    """Tokens hunter's own job history knows about that a probe reading
    doesn't reflect yet -- kept as two SEPARATE fields, never one shared
    number, because anthropic:5h and anthropic:7d have their own,
    generally DIFFERENT probe recency (5h rolls over ~33.6x more often
    than 7d, so after almost any 5h rollover the two have diverged) --
    collapsing them into a single int and deriving one from the other by
    a capacity ratio silently assumes they share a baseline, which is
    false the moment either window's own probe timing moves independently
    of the other's. Making this two fields instead of one int is the
    actual fix: budget.decide() can no longer receive an ambiguous
    number and misapply it to the wrong window -- the caller is forced
    to say, by name, which window each count is for."""

    for_5h: int = 0
    for_7d: int = 0


@dataclass
class BudgetDecision:
    allow: bool
    reason: str
    cap_tokens: int = 0  # effective per-job cap when allowed
    retry_at: float | None = None  # epoch ms: best-known time this could change
