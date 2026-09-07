"""Ingest a hunt worker's findings.json into the store, deduplicating."""

from __future__ import annotations

import json
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .types import BUG_CLASSES, SEVERITIES, Row

if TYPE_CHECKING:
    from .store import Store


_KNOWN_FINDING_TYPES = ("bug", "dep_update", "test_gap", "refactor", "modernization")

# Storage columns that are specific to each non-bug finding type (see
# store.upsert_finding's INSERT column list) and that the corresponding
# playbook (hunter/playbooks/<type>.md, "Output contract" section)
# instructs workers to always emit. A worker that omits one of these
# would otherwise be stored with that column silently NULL. `missing_tests`
# is the one list-shaped field (store.upsert_finding only json.dumps()s it
# when isinstance(..., list) -- a dict or other truthy-but-wrong-shaped
# value passes an isinstance-blind truthy check, then hits SQLite's binder
# unconverted and raises, aborting every later entry in the same batch);
# every other required field is a plain string.
_TYPE_REQUIRED_FIELDS: dict[str, tuple[str, ...]] = {
    "dep_update": ("ecosystem", "package", "current_version", "latest_version", "update_type"),
    "test_gap": ("missing_tests", "test_file"),
    "refactor": ("smell_type", "suggested_refactor"),
    "modernization": ("modernization_class", "current_approach", "proposed_approach"),
}
_LIST_REQUIRED_FIELDS = frozenset({"missing_tests"})


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
        for field in _TYPE_REQUIRED_FIELDS.get(finding_type, ()):
            value = f.get(field)
            if field in _LIST_REQUIRED_FIELDS:
                if not isinstance(value, list) or not value:
                    return (
                        f"required field {field!r} for type {finding_type!r}"
                        " must be a non-empty list"
                    )
            elif not isinstance(value, str) or not value:
                return (
                    f"required field {field!r} for type {finding_type!r}"
                    " must be a non-empty string"
                )
    try:
        float(f.get("confidence", 0.0))
    except (TypeError, ValueError):
        return "non-numeric confidence"
    return None
