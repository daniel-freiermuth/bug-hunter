"""Ingest a hunt worker's findings.json into the store, deduplicating."""
from __future__ import annotations

import json
from pathlib import Path

from .types import BUG_CLASSES

_SEVERITIES = ("high", "medium", "low")


def ingest_findings(store, repo_id: int, findings_path: Path) -> dict:
    result = {"inserted": 0, "duplicates": 0, "invalid": 0}
    try:
        entries = json.loads(Path(findings_path).read_text())
    except (OSError, json.JSONDecodeError) as e:
        store.log_event("error", f"ingest: unreadable findings file {findings_path}: {e}")
        result["invalid"] += 1
        return result
    if not isinstance(entries, list):
        store.log_event("error", f"ingest: findings root is not a list in {findings_path}")
        result["invalid"] += 1
        return result

    for i, f in enumerate(entries):
        problem = _validate(f)
        if problem:
            result["invalid"] += 1
            store.log_event("error", f"ingest: entry {i} invalid ({problem}): {json.dumps(f)[:300]}")
            continue
        f = dict(f)
        f["confidence"] = max(0.0, min(1.0, float(f.get("confidence", 0.0))))
        fid, inserted = store.upsert_finding(repo_id, f)
        if inserted:
            result["inserted"] += 1
            store.log_event("hunt", f"new finding: {f['fingerprint']}", finding_id=fid)
        else:
            result["duplicates"] += 1
    return result


def _validate(f) -> str | None:
    if not isinstance(f, dict):
        return "not an object"
    if not f.get("fingerprint"):
        return "missing fingerprint"
    if f.get("bug_class") not in BUG_CLASSES:
        return f"unknown bug_class {f.get('bug_class')!r}"
    if f.get("severity") not in _SEVERITIES:
        return f"unknown severity {f.get('severity')!r}"
    try:
        float(f.get("confidence", 0.0))
    except (TypeError, ValueError):
        return "non-numeric confidence"
    return None
