"""SQLite store — repos, findings, jobs, events, window log."""
from __future__ import annotations

import sqlite3
from pathlib import Path

from .types import (
    ACTIVE_STATUSES,
    FINDING_STATUSES,
    SCHEMA_PATH,
    SUPPRESSED_STATUSES,
    Config,
    WindowState,
    now_ms,
)

_JOB_COLUMNS = {
    "state", "pid", "session_file", "cap_tokens", "tokens_new", "calls",
    "exit_code", "killed_reason", "notes", "started_at", "finished_at",
    "finding_id",
}

_PR_STATE_COLUMNS = {
    "pr_number", "state", "mergeable", "checks", "head_ref",
    "last_activity_at", "last_engaged_activity_at", "needs_attention",
    "synced_at",
}

_FINDING_KEYS = (
    "fingerprint", "file", "symbol", "line", "bug_class", "severity",
    "confidence", "summary", "detail", "evidence_plan", "introduced_by",
)


def _rows(cur) -> list[dict]:
    return [dict(r) for r in cur.fetchall()]


class Store:
    def __init__(self, cfg: Config):
        cfg.db_path.parent.mkdir(parents=True, exist_ok=True)
        self.db = sqlite3.connect(cfg.db_path)
        self.db.row_factory = sqlite3.Row
        self.db.executescript(SCHEMA_PATH.read_text())
        self.db.commit()

    # -- repos ---------------------------------------------------------
    def add_repo(self, name: str, url: str, path: str, default_branch: str = "main") -> int:
        cur = self.db.execute(
            "INSERT INTO repos (name, url, path, default_branch, added_at) VALUES (?,?,?,?,?)",
            (name, url, str(path), default_branch, now_ms()),
        )
        self.db.commit()
        return cur.lastrowid

    def get_repo(self, key) -> dict | None:
        q = "id = ?" if isinstance(key, int) or str(key).isdigit() else "name = ?"
        cur = self.db.execute(f"SELECT * FROM repos WHERE {q}", (key,))
        r = cur.fetchone()
        return dict(r) if r else None

    def list_repos(self) -> list[dict]:
        return _rows(self.db.execute("SELECT * FROM repos ORDER BY name"))

    def set_last_hunt(self, repo_id: int, sha: str) -> None:
        self.db.execute(
            "UPDATE repos SET last_hunt_sha = ?, last_hunt_at = ? WHERE id = ?",
            (sha, now_ms(), repo_id),
        )
        self.db.commit()

    # -- findings ------------------------------------------------------
    def upsert_finding(self, repo_id: int, f: dict) -> tuple[int, bool]:
        cur = self.db.execute(
            "SELECT id FROM findings WHERE fingerprint = ?", (f["fingerprint"],)
        )
        row = cur.fetchone()
        if row:
            return row["id"], False
        t = now_ms()
        cur = self.db.execute(
            "INSERT INTO findings (repo_id, fingerprint, file, symbol, line, bug_class,"
            " severity, confidence, summary, detail, evidence_plan, introduced_by,"
            " status, created_at, updated_at)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?,?, 'new', ?, ?)",
            (
                repo_id,
                f["fingerprint"], f.get("file", ""), f.get("symbol"), f.get("line"),
                f["bug_class"], f["severity"], float(f.get("confidence", 0.0)),
                f.get("summary", ""), f.get("detail"), f.get("evidence_plan"),
                f.get("introduced_by"), t, t,
            ),
        )
        self.db.commit()
        return cur.lastrowid, True

    def get_finding(self, fid: int) -> dict | None:
        r = self.db.execute("SELECT * FROM findings WHERE id = ?", (fid,)).fetchone()
        return dict(r) if r else None

    def list_findings(self, status: str | None = None, repo_id: int | None = None) -> list[dict]:
        q, args = "SELECT * FROM findings", []
        conds = []
        if status:
            conds.append("status = ?"); args.append(status)
        if repo_id:
            conds.append("repo_id = ?"); args.append(repo_id)
        if conds:
            q += " WHERE " + " AND ".join(conds)
        q += " ORDER BY id DESC"
        return _rows(self.db.execute(q, args))

    def set_status(self, fid: int, status: str, verdict_reason: str | None = None,
                   pr_url: str | None = None, rung: int | None = None) -> None:
        if status not in FINDING_STATUSES:
            raise ValueError(f"invalid status: {status}")
        sets, args = ["status = ?", "updated_at = ?"], [status, now_ms()]
        if verdict_reason is not None:
            sets.append("verdict_reason = ?"); args.append(verdict_reason)
        if pr_url is not None:
            sets.append("pr_url = ?"); args.append(pr_url)
        if rung is not None:
            sets.append("rung_achieved = ?"); args.append(rung)
        args.append(fid)
        self.db.execute(f"UPDATE findings SET {', '.join(sets)} WHERE id = ?", args)
        self.db.commit()

    def suppressions(self, repo_id: int) -> list[dict]:
        ph = ",".join("?" * len(SUPPRESSED_STATUSES))
        return _rows(self.db.execute(
            f"SELECT * FROM findings WHERE repo_id = ? AND status IN ({ph}) ORDER BY id",
            (repo_id, *SUPPRESSED_STATUSES),
        ))

    def known_active(self, repo_id: int) -> list[dict]:
        ph = ",".join("?" * len(ACTIVE_STATUSES))
        return _rows(self.db.execute(
            f"SELECT * FROM findings WHERE repo_id = ? AND status IN ({ph}) ORDER BY id",
            (repo_id, *ACTIVE_STATUSES),
        ))

    # -- pr state --------------------------------------------------------
    def get_pr_state(self, fid: int) -> dict | None:
        r = self.db.execute(
            "SELECT * FROM pr_state WHERE finding_id = ?", (fid,)).fetchone()
        return dict(r) if r else None

    def upsert_pr_state(self, fid: int, **fields) -> None:
        bad = set(fields) - _PR_STATE_COLUMNS
        if bad:
            raise ValueError(f"invalid pr_state fields: {bad}")
        cols = ", ".join(fields)
        ph = ",".join("?" * (len(fields) + 1))
        sets = ", ".join(f"{k} = excluded.{k}" for k in fields)
        self.db.execute(
            f"INSERT INTO pr_state (finding_id, {cols}) VALUES ({ph})"
            f" ON CONFLICT(finding_id) DO UPDATE SET {sets}",
            (fid, *fields.values()),
        )
        self.db.commit()

    def list_attention(self) -> list[dict]:
        """pr_open findings whose PR needs a response, stalest sync first."""
        return _rows(self.db.execute(
            "SELECT f.*, p.pr_number, p.head_ref, p.needs_attention, p.synced_at"
            " FROM findings f JOIN pr_state p ON p.finding_id = f.id"
            " WHERE p.needs_attention IS NOT NULL AND f.status = 'pr_open'"
            " ORDER BY p.synced_at"
        ))

    # -- jobs ----------------------------------------------------------
    def create_job(self, kind: str, repo_id: int, finding_id: int | None = None,
                   cap_tokens: int | None = None) -> int:
        cur = self.db.execute(
            "INSERT INTO jobs (kind, repo_id, finding_id, cap_tokens, state, started_at)"
            " VALUES (?,?,?,?, 'queued', ?)",
            (kind, repo_id, finding_id, cap_tokens, now_ms()),
        )
        self.db.commit()
        return cur.lastrowid

    def update_job(self, job_id: int, **fields) -> None:
        bad = set(fields) - _JOB_COLUMNS
        if bad:
            raise ValueError(f"invalid job fields: {bad}")
        sets = ", ".join(f"{k} = ?" for k in fields)
        self.db.execute(f"UPDATE jobs SET {sets} WHERE id = ?", (*fields.values(), job_id))
        self.db.commit()

    def list_jobs(self, limit: int = 50) -> list[dict]:
        return _rows(self.db.execute(
            "SELECT j.*, r.name AS repo_name FROM jobs j JOIN repos r ON r.id = j.repo_id"
            " ORDER BY j.id DESC LIMIT ?", (limit,),
        ))

    # -- events / window log -------------------------------------------
    def log_event(self, kind: str, message: str, job_id: int | None = None,
                  finding_id: int | None = None) -> None:
        self.db.execute(
            "INSERT INTO events (at, kind, message, job_id, finding_id) VALUES (?,?,?,?,?)",
            (now_ms(), kind, message, job_id, finding_id),
        )
        self.db.commit()

    def recent_events(self, limit: int = 100) -> list[dict]:
        return _rows(self.db.execute(
            "SELECT * FROM events ORDER BY id DESC LIMIT ?", (limit,)))

    def log_window(self, states: list[WindowState]) -> None:
        t = now_ms()
        for w in states:
            self.db.execute(
                "INSERT INTO window_log (observed_at, limit_id, used_fraction, status,"
                " resets_at, source_age_s) VALUES (?,?,?,?,?,?)",
                (t, w.limit_id, w.used_fraction, w.status, w.resets_at, int(w.age_s)),
            )
        self.db.commit()

    def update_finding_analysis(self, fid: int, summary: str | None = None,
                                detail: str | None = None,
                                confidence: float | None = None,
                                severity: str | None = None) -> None:
        """Update analysis fields only (summary/detail/confidence/severity).
        Never touches status or verdict_reason."""
        sets: list[str] = ["updated_at = ?"]
        args: list = [now_ms()]
        if summary is not None:
            sets.append("summary = ?"); args.append(summary)
        if detail is not None:
            sets.append("detail = ?"); args.append(detail)
        if confidence is not None:
            sets.append("confidence = ?"); args.append(float(confidence))
        if severity is not None:
            sets.append("severity = ?"); args.append(severity)
        args.append(fid)
        self.db.execute(f"UPDATE findings SET {', '.join(sets)} WHERE id = ?", args)
        self.db.commit()
