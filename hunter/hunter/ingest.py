"""Ingest a hunt worker's findings.json into the store, deduplicating."""

from __future__ import annotations

import json
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .types import BUG_CLASSES, SEVERITIES, Row

if TYPE_CHECKING:
    from .store import Store


_KNOWN_FINDING_TYPES = ("bug", "dep_update", "test_gap", "refactor", "modernization")


def ingest_findings(
    store: Store, repo_id: int, findings_path: Path, finding_type: str | None = "bug"
) -> dict[str, int]:
    """finding_type=None means every entry in the file declares its own
    "type" (used for FOLLOW-UPS.json, which can propose any kind of
    follow-up, not just one fixed type per file -- see
    hunter.scheduler's ingestion call sites)."""
    result: dict[str, int] = {"inserted": 0, "duplicates": 0, "invalid": 0}
    try:
        entries = json.loads(Path(findings_path).read_text())
    except (OSError, json.JSONDecodeError) as e:
        store.log_event("error", f"ingest: unreadable findings file {findings_path}: {e}")
        result["invalid"] += 1
        return result
    if not isinstance(entries, list):
        store.log_event(
            "error",
            f"ingest: findings root is not a list in {findings_path}",
        )
        result["invalid"] += 1
        return result

    for i, f in enumerate(entries):
        entry_type = finding_type if finding_type is not None else (
            f.get("type") if isinstance(f, dict) else None
        )
        if entry_type not in _KNOWN_FINDING_TYPES:
            result["invalid"] += 1
            store.log_event(
                "error",
                f"ingest: entry {i} has unknown/missing type {entry_type!r}: {json.dumps(f)[:300]}",
            )
            continue
        problem = _validate(f, entry_type)
        if problem:
            result["invalid"] += 1
            store.log_event(
                "error",
                f"ingest: entry {i} invalid ({problem}): {json.dumps(f)[:300]}",
            )
            continue
        row: Row = dict(f)
        row["confidence"] = max(0.0, min(1.0, float(row.get("confidence", 0.0))))
        fid, inserted = store.upsert_finding(repo_id, row, finding_type=entry_type)
        event_kind = "hunt" if entry_type == "bug" else entry_type
        if inserted:
            result["inserted"] += 1
            store.log_event(event_kind, f"new {entry_type}: {row['fingerprint']}", finding_id=fid)
        else:
            result["duplicates"] += 1
    return result


def _validate(f: Any, finding_type: str) -> str | None:
    if not isinstance(f, dict):
        return "not an object"
    if not f.get("fingerprint"):
        return "missing fingerprint"
    if finding_type == "bug":
        if f.get("bug_class") not in BUG_CLASSES:
            return f"unknown bug_class {f.get('bug_class')!r}"
        if f.get("severity") not in SEVERITIES:
            return f"unknown severity {f.get('severity')!r}"
    else:
        sev = f.get("severity", "medium")
        if sev not in SEVERITIES:
            return f"unknown severity {sev!r}"
    try:
        float(f.get("confidence", 0.0))
    except (TypeError, ValueError):
        return "non-numeric confidence"
    return None
