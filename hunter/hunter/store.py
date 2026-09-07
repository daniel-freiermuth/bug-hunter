"""SQLite store -- repos, findings, jobs, events, window log."""

from __future__ import annotations

import contextlib
import json
import sqlite3
from typing import TYPE_CHECKING, ClassVar, cast

if TYPE_CHECKING:
    from collections.abc import Iterator

from .types import (
    ACTIVE_STATUSES,
    FINDING_STATUSES,
    SCHEMA_PATH,
    SUPPRESSED_STATUSES,
    Config,
    EventDict,
    JobDict,
    Row,
    SchedulerStateDict,
    Severity,
    WindowState,
    now_ms,
)

# The value types SQLite actually stores for every column these dynamic
# setters (update_repo/update_job/upsert_pr_state) and query-parameter
# builders (list_findings/list_all_findings/set_status/
# update_finding_analysis) touch -- str/int/float per schema.sql, plus
# None for nullable columns. Precise enough to catch a real type error
# (e.g. passing a dict or a list by mistake) while still fitting every
# legitimate column value, without Any's "stop checking entirely."
SqlParam = str | int | float | None

_JOB_COLUMNS = {
    "state",
    "pid",
    "session_file",
    "cap_tokens",
    "tokens_new",
    "calls",
    "exit_code",
    "killed_reason",
    "notes",
    "model",
    "usage_delta",
    "started_at",
    "finished_at",
    "finding_id",
}

_PR_STATE_COLUMNS = {
    "pr_number",
    "state",
    "mergeable",
    "checks",
    "head_ref",
    "last_activity_at",
    "last_engaged_activity_at",
    "needs_attention",
    "synced_at",
    "harvested_at",
}

_FINDING_KEYS = (
    "fingerprint",
    "file",
    "symbol",
    "line",
    "bug_class",
    "severity",
    "confidence",
    "summary",
    "detail",
    "evidence_plan",
    "introduced_by",
)


def _rows(cur: sqlite3.Cursor) -> list[Row]:
    return [dict(r) for r in cur.fetchall()]


def _require_keys[T](row: Row, *required: str, shape: type[T]) -> T:
    """Verify a raw SQLite row has every key a TypedDict declares before
    asserting that type onto it. This is the one runtime check standing
    between "the SQL query's actual columns" (a fact only the database
    knows) and "the Python type system's belief about that shape" (a
    fact only the TypedDict declares) -- the seam neither mypy nor any
    static checker can verify on its own, since a query's result shape
    isn't visible to the type checker. A schema/query drift now fails
    loudly, here, with the exact missing key and what was actually
    present, on the very first call that hits it -- not as a KeyError
    deep inside unrelated code far from the real defect.
    """
    missing = [k for k in required if k not in row]
    if missing:
        msg = f"row missing required keys {missing} for {shape.__name__}: got {sorted(row)}"
        raise ValueError(msg)
    return cast("T", row)


class Store:
    def __init__(self, cfg: Config) -> None:
        self.cfg = cfg
        cfg.db_path.parent.mkdir(parents=True, exist_ok=True)
        self.db = sqlite3.connect(cfg.db_path)
        self.db.row_factory = sqlite3.Row
        self.db.executescript(SCHEMA_PATH.read_text())
        self.db.commit()
        # Migrations for existing DBs.
        for col, tbl, sql in [
            ("forge", "repos", "ALTER TABLE repos ADD COLUMN forge TEXT NOT NULL DEFAULT 'github'"),
            ("model", "jobs", "ALTER TABLE jobs ADD COLUMN model TEXT"),
            ("usage_delta", "jobs", "ALTER TABLE jobs ADD COLUMN usage_delta REAL"),
            ("budget_override", "findings", "ALTER TABLE findings ADD COLUMN budget_override TEXT"),
            ("last_full_hunt_at", "repos", "ALTER TABLE repos ADD COLUMN last_full_hunt_at INTEGER"),
            ("last_test_gap_at", "repos", "ALTER TABLE repos ADD COLUMN last_test_gap_at INTEGER"),
            ("last_dep_update_at", "repos", "ALTER TABLE repos ADD COLUMN last_dep_update_at INTEGER"),
            ("last_refactor_at", "repos", "ALTER TABLE repos ADD COLUMN last_refactor_at INTEGER"),
            ("last_modernization_at", "repos", "ALTER TABLE repos ADD COLUMN last_modernization_at INTEGER"),
            ("modernization_class", "findings", "ALTER TABLE findings ADD COLUMN modernization_class TEXT"),
            ("current_approach", "findings", "ALTER TABLE findings ADD COLUMN current_approach TEXT"),
            ("proposed_approach", "findings", "ALTER TABLE findings ADD COLUMN proposed_approach TEXT"),
            ("harvested_at", "pr_state", "ALTER TABLE pr_state ADD COLUMN harvested_at INTEGER"),
        ]:
            try:
                self.db.execute(f"SELECT {col} FROM {tbl} LIMIT 1")
            except sqlite3.OperationalError:
                self.db.execute(sql)
                self.db.commit()

    # -- repos ---------------------------------------------------------
    def add_repo(
        self,
        name: str,
        url: str,
        path: str,
        default_branch: str = "main",
        forge: str = "github",
    ) -> int:
        cur = self.db.execute(
            "INSERT INTO repos (name, url, path, forge, default_branch, added_at)"
            " VALUES (?,?,?,?,?,?)",
            (name, url, str(path), forge, default_branch, now_ms()),
        )
        self.db.commit()
        assert cur.lastrowid is not None
        return cur.lastrowid

    def get_repo(self, key: int | str) -> Row | None:
        q = "id = ?" if isinstance(key, int) or str(key).isdigit() else "name = ?"
        cur = self.db.execute(f"SELECT * FROM repos WHERE {q}", (key,))
        r = cur.fetchone()
        return dict(r) if r else None

    def list_repos(self) -> list[Row]:
        return _rows(self.db.execute("SELECT * FROM repos ORDER BY name"))

    def set_last_hunt(self, repo_id: int, sha: str) -> None:
        self.db.execute(
            "UPDATE repos SET last_hunt_sha = ?, last_hunt_at = ? WHERE id = ?",
            (sha, now_ms(), repo_id),
        )
        self.db.commit()

    def update_repo(self, repo_id: int, **fields: str | int) -> None:
        allowed = {"name", "url", "default_branch", "forge", "enabled"}
        bad = set(fields) - allowed
        if bad:
            msg = f"invalid repo fields: {bad}"
            raise ValueError(msg)
        if not fields:
            return
        sets = ", ".join(f"{k} = ?" for k in fields)
        self.db.execute(
            f"UPDATE repos SET {sets} WHERE id = ?",
            (*fields.values(), repo_id),
        )
        self.db.commit()

    def delete_repo(self, repo_id: int) -> None:
        """Remove a repo record. Refuses if findings or jobs still reference
        it -- delete their history first, or use update_repo(enabled=False)
        to pause scheduling without losing data."""
        n_findings = self.db.execute(
            "SELECT COUNT(*) AS n FROM findings WHERE repo_id = ?", (repo_id,)
        ).fetchone()["n"]
        n_jobs = self.db.execute("SELECT COUNT(*) AS n FROM jobs WHERE repo_id = ?", (repo_id,)).fetchone()["n"]
        if n_findings or n_jobs:
            msg = (
                f"repo {repo_id} has {n_findings} finding(s) and {n_jobs} job(s) -- "
                "cannot delete without losing history; pause it instead"
            )
            raise ValueError(msg)
        self.db.execute("DELETE FROM repos WHERE id = ?", (repo_id,))
        self.db.commit()

    # -- repo notes ----------------------------------------------------
    def repo_notes_path(self, repo_id: int) -> Path:
        """Return path to repo's NOTES.md file (ID-based for stability)."""
        from pathlib import Path as PathType
        
        repo = self.get_repo(repo_id)
        if not repo:
            msg = f"repo {repo_id} not found"
            raise ValueError(msg)
        # Use repo_id for path stability (survives renames)
        return self.cfg.work_root / "repos" / f"repo-{repo_id}" / "NOTES.md"

    def repo_notes(self, repo_id: int) -> str:
        """Read repo notes, or empty string if none exist."""
        p = self.repo_notes_path(repo_id)
        return p.read_text() if p.exists() else ""

    def append_repo_note(self, repo_id: int, note: str, category: str | None = None) -> None:
        """Append timestamped note to repo's NOTES.md."""
        from datetime import datetime

        p = self.repo_notes_path(repo_id)
        p.parent.mkdir(parents=True, exist_ok=True)
        
        # If file doesn't exist, create with header
        if not p.exists():
            repo = self.get_repo(repo_id)
            p.write_text(f"# Notes: {repo['name']}\n\nLast updated: {datetime.now().date()}\n\n")
        
        # Append note
        with p.open("a") as f:
            ts = datetime.now().strftime("%Y-%m-%d %H:%M")
            if category:
                f.write(f"## {category}\n")
            f.write(f"- [{ts}] {note}\n\n")

    # -- findings ------------------------------------------------------
    def upsert_finding(self, repo_id: int, f: Row, finding_type: str = "bug") -> tuple[int, bool]:
        cur = self.db.execute("SELECT id FROM findings WHERE fingerprint = ?", (f["fingerprint"],))
        row = cur.fetchone()
        if row:
            return int(row["id"]), False
        t = now_ms()
        missing_tests = f.get("missing_tests")
        if isinstance(missing_tests, list):
            missing_tests = json.dumps(missing_tests)
        cur = self.db.execute(
            "INSERT INTO findings (type, repo_id, fingerprint, file, symbol, line, bug_class,"
            " severity, confidence, summary, detail, evidence_plan, introduced_by,"
            " ecosystem, package, current_version, latest_version, update_type, security_advisory,"
            " missing_tests, test_file, smell_type, suggested_refactor,"
            " modernization_class, current_approach, proposed_approach,"
            " status, created_at, updated_at)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, 'new', ?, ?)",
            (
                finding_type,
                repo_id,
                f["fingerprint"],
                f.get("file", ""),
                f.get("symbol"),
                f.get("line"),
                f.get("bug_class"),
                f.get("severity", "medium"),
                float(f.get("confidence", 0.0)),
                f.get("summary", ""),
                f.get("detail"),
                f.get("evidence_plan"),
                f.get("introduced_by"),
                f.get("ecosystem"),
                f.get("package"),
                f.get("current_version"),
                f.get("latest_version"),
                f.get("update_type"),
                f.get("security_advisory"),
                missing_tests,
                f.get("test_file"),
                f.get("smell_type"),
                f.get("suggested_refactor"),
                f.get("modernization_class"),
                f.get("current_approach"),
                f.get("proposed_approach"),
                t,
                t,
            ),
        )
        self.db.commit()
        assert cur.lastrowid is not None
        return cur.lastrowid, True

    def get_finding(self, fid: int) -> Row | None:
        r = self.db.execute("SELECT * FROM findings WHERE id = ?", (fid,)).fetchone()
        return dict(r) if r else None

    def list_findings(
        self,
        status: str | None = None,
        repo_id: int | None = None,
        min_severity: str | None = None,
    ) -> list[Row]:
        q = "SELECT * FROM findings"
        args: list[SqlParam] = []
        conds: list[str] = []
        if status:
            conds.append("status = ?")
            args.append(status)
        if repo_id:
            conds.append("repo_id = ?")
            args.append(repo_id)
        if min_severity:
            allowed = Severity.at_or_above(Severity.from_str(min_severity))
            ph = ",".join("?" * len(allowed))
            conds.append(f"severity IN ({ph})")
            args.extend(allowed)
        if conds:
            q += " WHERE " + " AND ".join(conds)
        q += " ORDER BY id DESC"
        return _rows(self.db.execute(q, args))

    def list_all_findings(
        self,
        status: str | None = None,
        repo_id: int | None = None,
        min_severity: str | None = None,
        finding_type: str | None = None,
    ) -> list[Row]:
        """
        Query findings across all types (bug, dep_update, test_gap, refactor).
        Each result has a 'type' field and a computed 'category' field for UI
        display (bug_class | update_type | 'coverage' | smell_type).
        """
        conds: list[str] = []
        args: list[SqlParam] = []
        if status:
            conds.append("status = ?")
            args.append(status)
        if repo_id:
            conds.append("repo_id = ?")
            args.append(repo_id)
        if finding_type:
            conds.append("type = ?")
            args.append(finding_type)
        if min_severity:
            allowed = Severity.at_or_above(Severity.from_str(min_severity))
            ph = ",".join("?" * len(allowed))
            conds.append(f"severity IN ({ph})")
            args.extend(allowed)
        q = "SELECT * FROM findings"
        if conds:
            q += " WHERE " + " AND ".join(conds)
        q += " ORDER BY id DESC"
        rows = _rows(self.db.execute(q, args))
        for r in rows:
            if r["type"] == "bug":
                r["category"] = r.get("bug_class")
            elif r["type"] == "dep_update":
                r["category"] = r.get("update_type")
            elif r["type"] == "test_gap":
                r["category"] = "coverage"
            elif r["type"] == "refactor":
                r["category"] = r.get("smell_type")
            elif r["type"] == "modernization":
                r["category"] = r.get("modernization_class")
        return rows

    def set_status(
        self,
        fid: int,
        status: str,
        verdict_reason: str | None = None,
        pr_url: str | None = None,
        rung: int | None = None,
    ) -> None:
        if status not in FINDING_STATUSES:
            msg = f"invalid status: {status}"
            raise ValueError(msg)
        sets: list[str] = ["status = ?", "updated_at = ?"]
        args: list[SqlParam] = [status, now_ms()]
        if verdict_reason is not None:
            sets.append("verdict_reason = ?")
            args.append(verdict_reason)
        if pr_url is not None:
            sets.append("pr_url = ?")
            args.append(pr_url)
        if rung is not None:
            sets.append("rung_achieved = ?")
            args.append(rung)
        args.append(fid)
        self.db.execute(f"UPDATE findings SET {', '.join(sets)} WHERE id = ?", args)
        self.db.commit()

    @contextlib.contextmanager
    def in_progress(self, fid: int, status: str, fallback: str) -> Iterator[None]:
        """Structural guard for a status that must never be observed
        outside the dynamic extent of the wrapped block (e.g. 'fixing' --
        the normal work queue never scans for it, so leaving a finding
        there is a silent, indefinite disappearance).

        Sets `status` on entry. On exit -- normal return, early return,
        OR an exception raised anywhere in the block -- checks whether
        anything already moved the finding to a DIFFERENT status; if
        not, forces it to `fallback`. This makes "the function raised
        before reaching its own terminal set_status call" structurally
        indistinguishable, in effect on the finding, from "the function
        finished and explicitly chose to requeue" -- there is no code
        path left that both (a) leaves `status` set and (b) exits the
        block, because Python guarantees `finally` runs on every exit
        including via exception or `return`.

        Does NOT cover the process itself being killed (SIGKILL, a hard
        crash) -- `finally` cannot run then. That residual case is what
        Store.reconcile_orphaned_jobs() exists for; this context manager
        is what makes everything short of total process death (which is
        the common case: any exception in git/network/file-IO code)
        impossible to get wrong, rather than merely repaired after.
        """
        self.set_status(fid, status)
        try:
            yield
        finally:
            current = self.get_finding(fid)
            if current is not None and current["status"] == status:
                self.set_status(fid, fallback)

    def set_budget_override(self, fid: int, mode: str | None) -> None:
        """Set budget override: 'once', 'exempt', or None to clear."""
        if mode not in (None, "once", "exempt"):
            msg = f"invalid budget_override mode: {mode!r}"
            raise ValueError(msg)
        self.db.execute(
            "UPDATE findings SET budget_override = ?, updated_at = ? WHERE id = ?",
            (mode, now_ms(), fid),
        )
        self.db.commit()

    def clear_all_overrides(self) -> int:
        """Clear all budget overrides. Returns count of affected rows."""
        cur = self.db.execute(
            "UPDATE findings SET budget_override = NULL, updated_at = ?"
            " WHERE budget_override IS NOT NULL",
            (now_ms(),),
        )
        self.db.commit()
        return cur.rowcount

    def suppressions(self, repo_id: int, finding_type: str = "bug") -> list[Row]:
        ph = ",".join("?" * len(SUPPRESSED_STATUSES))
        return _rows(
            self.db.execute(
                f"SELECT * FROM findings WHERE repo_id = ? AND type = ? AND status IN ({ph}) ORDER BY id",
                (repo_id, finding_type, *SUPPRESSED_STATUSES),
            )
        )

    def known_active(self, repo_id: int, finding_type: str = "bug") -> list[Row]:
        ph = ",".join("?" * len(ACTIVE_STATUSES))
        return _rows(
            self.db.execute(
                f"SELECT * FROM findings WHERE repo_id = ? AND type = ? AND status IN ({ph}) ORDER BY id",
                (repo_id, finding_type, *ACTIVE_STATUSES),
            )
        )

    # -- pr state --------------------------------------------------------
    def get_pr_state(self, fid: int) -> Row | None:
        r = self.db.execute("SELECT * FROM pr_state WHERE finding_id = ?", (fid,)).fetchone()
        return dict(r) if r else None

    def upsert_pr_state(self, fid: int, **fields: SqlParam) -> None:
        bad = set(fields) - _PR_STATE_COLUMNS
        if bad:
            msg = f"invalid pr_state fields: {bad}"
            raise ValueError(msg)
        cols = ", ".join(fields)
        ph = ",".join("?" * (len(fields) + 1))
        sets = ", ".join(f"{k} = excluded.{k}" for k in fields)
        self.db.execute(
            f"INSERT INTO pr_state (finding_id, {cols}) VALUES ({ph})"
            f" ON CONFLICT(finding_id) DO UPDATE SET {sets}",
            (fid, *fields.values()),
        )
        self.db.commit()

    def list_attention(self) -> list[Row]:
        """pr_open findings whose PR needs a response, stalest sync first."""
        return _rows(
            self.db.execute(
                "SELECT f.*, p.pr_number, p.head_ref, p.needs_attention, p.synced_at"
                " FROM findings f JOIN pr_state p ON p.finding_id = f.id"
                " WHERE p.needs_attention IS NOT NULL AND f.status = 'pr_open'"
                " ORDER BY p.synced_at"
            )
        )

    def list_pending_harvest(self) -> list[Row]:
        """Merged findings whose PR hasn't yet been reviewed for follow-up
        work, oldest merge first -- run_harvest's queue. A PR's true final
        scope is only known once merged (see run_harvest's docstring for
        why this is a separate pass from run_fix's ship-time snapshot and
        run_engage's withdraw-time one), so this stays open until that
        review actually happens, however long after the merge that is."""
        return _rows(
            self.db.execute(
                "SELECT f.*, p.pr_number, p.head_ref, p.synced_at"
                " FROM findings f JOIN pr_state p ON p.finding_id = f.id"
                " WHERE f.status = 'merged' AND p.harvested_at IS NULL"
                " ORDER BY p.synced_at"
            )
        )

    # -- jobs ----------------------------------------------------------
    def create_job(
        self,
        kind: str,
        repo_id: int,
        finding_id: int | None = None,
        cap_tokens: int | None = None,
        state: str = "queued",
    ) -> int:
        """'queued' is a transient default a caller immediately overwrites
        (via update_job) with the real outcome -- nothing ever reads a job
        row while it's actually 'queued'. Pass state="running" directly
        for jobs that are about to run: current_job() (the Status page's
        "what's happening" panel) filters on state='running', and leaving
        a row at the 'queued' default during the real prep work between
        create_job and the old separate update_job(state="running") call
        (prompt building, git operations, worktree setup) opened a real
        window where _cycle_lock was already held (the UI's "cycle
        running" indicator) but current_job() found nothing yet, showing
        stale last-cycle text instead."""
        cur = self.db.execute(
            "INSERT INTO jobs (kind, repo_id, finding_id, cap_tokens, state, started_at)"
            " VALUES (?,?,?,?,?,?)",
            (kind, repo_id, finding_id, cap_tokens, state, now_ms()),
        )
        self.db.commit()
        assert cur.lastrowid is not None
        return cur.lastrowid

    def update_job(self, job_id: int, **fields: SqlParam) -> None:
        bad = set(fields) - _JOB_COLUMNS
        if bad:
            msg = f"invalid job fields: {bad}"
            raise ValueError(msg)
        sets = ", ".join(f"{k} = ?" for k in fields)
        self.db.execute(f"UPDATE jobs SET {sets} WHERE id = ?", (*fields.values(), job_id))
        self.db.commit()

    def list_jobs(self, limit: int = 50) -> list[Row]:
        return _rows(
            self.db.execute(
                "SELECT j.*, r.name AS repo_name FROM jobs j"
                " JOIN repos r ON r.id = j.repo_id"
                " ORDER BY j.id DESC LIMIT ?",
                (limit,),
            )
        )

    def jobs_by_finding(self, fid: int) -> list[Row]:
        """Every job ever run against this finding, newest first -- unlike
        list_jobs()'s recent-50 window, this is the complete history for
        one finding (a finding fixed weeks ago can have jobs long since
        pushed off that global feed). Powers the /api/finding detail
        view's "all measurements" panel."""
        return _rows(
            self.db.execute(
                "SELECT j.*, r.name AS repo_name FROM jobs j"
                " JOIN repos r ON r.id = j.repo_id"
                " WHERE j.finding_id = ? ORDER BY j.id DESC",
                (fid,),
            )
        )

    def current_job(self) -> JobDict | None:
        """The job currently in flight, if any -- at most one, given
        _cycle_lock serializes the daemon loop and POST /api/cycle."""
        r = self.db.execute(
            "SELECT j.*, r.name AS repo_name FROM jobs j"
            " JOIN repos r ON r.id = j.repo_id"
            " WHERE j.state = 'running' ORDER BY j.id DESC LIMIT 1"
        ).fetchone()
        if r is None:
            return None
        row = dict(r)
        if row.get("finding_id"):
            f = self.get_finding(row["finding_id"])
            if f is not None:
                row["finding_summary"] = f.get("summary")
                row["finding_fingerprint"] = f.get("fingerprint")
        return _require_keys(
            row,
            "id", "kind", "repo_id", "repo_name", "finding_id", "state", "pid",
            "session_file", "cap_tokens", "tokens_new", "calls", "exit_code",
            "killed_reason", "notes", "started_at", "finished_at", "model", "usage_delta",
            shape=JobDict,
        )

    def set_scheduler_state(self, state: str, detail: str, next_wake_at: int | None) -> None:
        """Persist the daemon loop's own read of "what am I doing and
        why" -- see schema.sql's scheduler_state comment. Informational
        only; never consulted by any scheduling decision."""
        self.db.execute(
            "INSERT INTO scheduler_state (id, state, detail, next_wake_at, updated_at)"
            " VALUES (1, ?, ?, ?, ?)"
            " ON CONFLICT(id) DO UPDATE SET state=excluded.state, detail=excluded.detail,"
            " next_wake_at=excluded.next_wake_at, updated_at=excluded.updated_at",
            (state, detail, next_wake_at, now_ms()),
        )
        self.db.commit()

    def get_scheduler_state(self) -> SchedulerStateDict | None:
        r = self.db.execute("SELECT * FROM scheduler_state WHERE id = 1").fetchone()
        if r is None:
            return None
        return _require_keys(
            dict(r), "id", "state", "detail", "next_wake_at", "updated_at", shape=SchedulerStateDict
        )

    def reconcile_orphaned_jobs(self) -> dict[str, list[Row]]:
        """Last-resort net for total process death (crash, systemctl
        restart, host reboot -- SIGKILL, or any signal that doesn't let
        Python's `finally` run). Any plain exception mid-run_fix is
        already handled structurally by `in_progress()`'s try/finally;
        this exists only for the residual case where the process itself
        stops executing, not just one function call.

        Safe to call unconditionally at the top of every cycle attempt:
        a single daemon process owns the job lifecycle exclusively (the
        systemd unit runs exactly one `hunter daemon`, and _cycle_lock
        serializes the loop against POST /api/cycle within that
        process), so at the moment THIS call runs -- before this process
        has started any new job this cycle -- nothing can legitimately
        still be mid-fix. Any finding found at 'fixing' here was
        orphaned by a process that died before `in_progress()`'s
        `finally` could run at all.

        Keys off findings.status directly, not jobs.state, for the same
        reason `in_progress()` does: a job row can already be terminal
        (written by _record_job) while the finding is still 'fixing' if
        the process died in the stretch after that but before
        `in_progress()`'s cleanup ran. 'fixing' is never scanned by the
        normal work queue (only queued/rechecking/pr_open-with-attention
        are), so without this it sits invisible and unretried
        indefinitely (observed in production: finding #57 sat stuck for
        a month after an old crash, before either mechanism existed).

        Job rows still 'running' are handled separately and marked
        'killed' so they stop inflating _unaccounted_tokens's running-job sum
        forever -- that's a real but lower-stakes leak (only makes the
        budget more conservative, doesn't strand any finding), so it is
        not required for the finding-status recovery above to work.

        Returns {"findings": [...], "jobs": [...]} -- the rows touched.
        """
        stuck_findings = _rows(
            self.db.execute("SELECT * FROM findings WHERE status = 'fixing'")
        )
        for f in stuck_findings:
            self.set_status(f["id"], "queued")

        orphaned_jobs = _rows(
            self.db.execute("SELECT * FROM jobs WHERE state = 'running'")
        )
        for r in orphaned_jobs:
            self.update_job(
                r["id"],
                state="killed",
                killed_reason="orphaned",
                finished_at=now_ms(),
                notes="reconciled at cycle startup -- prior process died mid-job",
            )
        return {"findings": stuck_findings, "jobs": orphaned_jobs}

    # -- events / window log -------------------------------------------
    def log_event(
        self,
        kind: str,
        message: str,
        job_id: int | None = None,
        finding_id: int | None = None,
    ) -> None:
        self.db.execute(
            "INSERT INTO events (at, kind, message, job_id, finding_id) VALUES (?,?,?,?,?)",
            (now_ms(), kind, message, job_id, finding_id),
        )
        self.db.commit()

    def recent_events(self, limit: int = 100) -> list[EventDict]:
        rows = _rows(
            self.db.execute("SELECT * FROM events ORDER BY id DESC LIMIT ?", (limit,))
        )
        return [
            _require_keys(r, "id", "at", "kind", "message", "job_id", "finding_id", shape=EventDict)
            for r in rows
        ]

    def events_by_finding(self, fids: list[int]) -> dict[int, list[Row]]:
        """Return events grouped by finding_id for the given IDs."""
        if not fids:
            return {}
        ph = ",".join("?" * len(fids))
        rows = _rows(
            self.db.execute(
                f"SELECT * FROM events WHERE finding_id IN ({ph}) ORDER BY id",
                fids,
            )
        )
        out: dict[int, list[Row]] = {}
        for r in rows:
            out.setdefault(r["finding_id"], []).append(r)
        return out

    _CALIBRATION_DURATIONS_MS: ClassVar[dict[str, int]] = {
        "5h": 5 * 3600 * 1000,
        "7d": 7 * 24 * 3600 * 1000,
    }

    def log_window(self, states: list[WindowState]) -> None:
        """Record each window observation, and -- whenever this is a
        FRESH probe (used_fraction actually moved since the last row for
        this exact window instance) -- also record a calibration sample
        correlating hunter's own token spend in that gap with how much
        Anthropic's used_fraction moved. See calibration_samples in
        schema.sql for what this is (and isn't) good for."""
        t = now_ms()
        for w in states:
            horizon = next(
                (h for h in self._CALIBRATION_DURATIONS_MS if f":{h}" in w.limit_id), None
            )
            if horizon and w.resets_at and w.used_fraction is not None:
                prev = self.db.execute(
                    "SELECT observed_at, used_fraction FROM window_log"
                    " WHERE limit_id = ? AND resets_at BETWEEN ? AND ?"
                    " ORDER BY observed_at DESC LIMIT 1",
                    (w.limit_id, w.resets_at - 5000, w.resets_at + 5000),
                ).fetchone()
                if (
                    prev is not None
                    and prev["used_fraction"] is not None
                    and w.used_fraction > prev["used_fraction"]
                    and (t - prev["observed_at"]) <= self._CALIBRATION_DURATIONS_MS[horizon]
                ):
                    tok = self.db.execute(
                        "SELECT COALESCE(SUM(tokens_new), 0) AS t FROM jobs"
                        " WHERE state != 'running' AND finished_at > ? AND finished_at <= ?",
                        (prev["observed_at"], t),
                    ).fetchone()["t"]
                    if tok > 0:
                        self.db.execute(
                            "INSERT INTO calibration_samples"
                            " (observed_at, limit_id, window_resets_at,"
                            " used_fraction_delta, hunter_tokens)"
                            " VALUES (?,?,?,?,?)",
                            (
                                t, w.limit_id, w.resets_at,
                                w.used_fraction - prev["used_fraction"], tok,
                            ),
                        )
            self.db.execute(
                "INSERT INTO window_log (observed_at, limit_id, used_fraction,"
                " status, resets_at, source_age_s) VALUES (?,?,?,?,?,?)",
                (t, w.limit_id, w.used_fraction, w.status, w.resets_at, int(w.age_s)),
            )
        self.db.commit()

    def estimate_capacity(
        self, limit_id: str, min_delta: float = 0.02, sample_limit: int = 200
    ) -> float | None:
        """Empirical estimate of this window's total token capacity, from
        accumulated calibration_samples -- tokens hunter itself spent
        divided by how much used_fraction moved in that same gap.

        Informational only (see calibration_samples in schema.sql): any
        concurrent non-hunter account activity inflates the observed
        used_fraction move without showing up in hunter_tokens, which can
        only push a sample's implied capacity DOWN, never up -- so the
        75th percentile across samples is a better estimate of the true
        capacity than the median, which is dragged down by however many
        samples happened to overlap other activity. min_delta discards
        tiny deltas dominated by Anthropic's own ~1%-quantized reporting.
        None if there isn't enough data yet to say anything.
        """
        rows = self.db.execute(
            "SELECT used_fraction_delta, hunter_tokens FROM calibration_samples"
            " WHERE limit_id = ? AND used_fraction_delta >= ?"
            " ORDER BY observed_at DESC LIMIT ?",
            (limit_id, min_delta, sample_limit),
        ).fetchall()
        ratios = sorted(float(r["hunter_tokens"]) / float(r["used_fraction_delta"]) for r in rows)
        if not ratios:
            return None
        return ratios[min(int(len(ratios) * 0.75), len(ratios) - 1)]

    def update_finding_analysis(
        self,
        fid: int,
        summary: str | None = None,
        detail: str | None = None,
        confidence: float | None = None,
        severity: str | None = None,
    ) -> None:
        """Update analysis fields only (summary/detail/confidence/severity).

        Never touches status or verdict_reason.
        """
        sets: list[str] = ["updated_at = ?"]
        args: list[SqlParam] = [now_ms()]
        if summary is not None:
            sets.append("summary = ?")
            args.append(summary)
        if detail is not None:
            sets.append("detail = ?")
            args.append(detail)
        if confidence is not None:
            sets.append("confidence = ?")
            args.append(float(confidence))
        if severity is not None:
            sets.append("severity = ?")
            args.append(severity)
        args.append(fid)
        self.db.execute(f"UPDATE findings SET {', '.join(sets)} WHERE id = ?", args)
        self.db.commit()

    # -- stats -------------------------------------------------------------
    def stats_by_kind(self) -> list[Row]:
        """Aggregate job stats grouped by kind."""
        return _rows(
            self.db.execute(
                "SELECT kind, COUNT(*) AS jobs,"
                " SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) AS done,"
                " SUM(CASE WHEN state='failed' THEN 1 ELSE 0 END) AS failed,"
                " SUM(CASE WHEN state='killed' THEN 1 ELSE 0 END) AS killed,"
                " SUM(CASE WHEN state='denied' THEN 1 ELSE 0 END) AS denied,"
                " SUM(tokens_new) AS total_tokens,"
                " SUM(calls) AS total_calls,"
                " AVG(tokens_new) AS avg_tokens,"
                " SUM(usage_delta) AS total_usage_delta,"
                " GROUP_CONCAT(DISTINCT model) AS models"
                " FROM jobs GROUP BY kind ORDER BY kind"
            )
        )

    def stats_by_finding(self) -> list[Row]:
        """Total tokens and job count per finding."""
        return _rows(
            self.db.execute(
                "SELECT j.finding_id, f.fingerprint, f.status, f.severity,"
                " COUNT(*) AS jobs,"
                " SUM(j.tokens_new) AS total_tokens,"
                " SUM(j.calls) AS total_calls,"
                " SUM(j.usage_delta) AS total_usage_delta"
                " FROM jobs j JOIN findings f ON f.id = j.finding_id"
                " WHERE j.finding_id IS NOT NULL"
                " GROUP BY j.finding_id"
                " ORDER BY total_tokens DESC"
            )
        )

    def stats_totals(self) -> Row:
        """Overall totals across all jobs."""
        rows = _rows(
            self.db.execute(
                "SELECT COUNT(*) AS jobs,"
                " SUM(tokens_new) AS total_tokens,"
                " SUM(calls) AS total_calls,"
                " SUM(usage_delta) AS total_usage_delta,"
                " SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) AS done,"
                " SUM(CASE WHEN state='denied' THEN 1 ELSE 0 END) AS denied"
                " FROM jobs"
            )
        )
        return rows[0] if rows else {}
