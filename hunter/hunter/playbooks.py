"""Render worker prompts from playbook templates."""

from __future__ import annotations

import json
from pathlib import Path

from .types import PLAYBOOK_DIR, Row

_FINDING_PROMPT_KEYS = (
    "type",
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
    # dep_update fields
    "ecosystem",
    "package",
    "current_version",
    "latest_version",
    "update_type",
    "security_advisory",
    # test_gap fields
    "missing_tests",
    "test_file",
    # refactor fields
    "smell_type",
    "suggested_refactor",
    # modernization fields
    "modernization_class",
    "current_approach",
    "proposed_approach",
)


def _render(template: str, slots: dict[str, object]) -> str:
    for k, v in slots.items():
        template = template.replace("{{" + k + "}}", str(v))
    assert "{{" not in template, (  # noqa: S101
        f"unfilled placeholder in playbook: {template[template.index('{{') :][:60]}"
    )
    return template


def _escape_braces(s: str) -> str:
    """Escape ``{{`` so _render's assertion doesn't fire on user content."""
    return str(s).replace("{{", "{ {")


def _suppressions_block(suppressions: list[Row]) -> str:
    """Rejected/wontfix corpus with the reason, so workers can recognize a
    still-applicable verdict instead of re-filing the same non-issue."""
    return (
        "\n".join(
            f"- {_escape_braces(s['fingerprint'])} -- {_escape_braces(s.get('verdict_reason') or '(no reason recorded)')}"
            for s in suppressions
        )
        or "(none yet)"
    )


def _known_block(known: list[Row]) -> str:
    """Active (non-terminal) findings, for novelty comparison."""
    return (
        "\n".join(f"- {_escape_braces(k['fingerprint'])} [{k['status']}] -- {_escape_braces(k.get('summary', ''))}" for k in known)
        or "(none yet)"
    )


def build_hunt_prompt(
    repo: Row,
    diff_range: str,
    scope_note: str,
    suppressions: list[Row],
    known: list[Row],
    out_path: Path,
    max_findings: int,
    repo_notes: str = "",
) -> str:
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet — consider adding conventions/architecture/gotchas as you discover them)"
    return _render(
        (PLAYBOOK_DIR / "hunt.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "DIFF_RANGE": diff_range,
            "SCOPE_NOTE": scope_note,
            "SUPPRESSIONS": _suppressions_block(suppressions),
            "KNOWN_FINDINGS": _known_block(known),
            "OUT_PATH": out_path,
            "MAX_FINDINGS": max_findings,
            "REPO_NOTES": notes,
        },
    )


def _finding_subset(finding: Row) -> dict[str, object]:
    """Prompt-relevant fields, dropping nulls (type-specific columns are
    nullable for other types)."""
    return {k: v for k in _FINDING_PROMPT_KEYS if (v := finding.get(k)) is not None}


def build_fix_prompt(finding: Row, worktree: Path, branch: str, repo: Row, repo_notes: str = "") -> str:
    subset = _finding_subset(finding)
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "fix.md").read_text(),
        {
            "WORKTREE": worktree,
            "BRANCH": branch,
            "FINDING_JSON": _escape_braces(json.dumps(subset, indent=2)),
            "REPO_NAME": repo["name"],
            "REPO_NOTES": notes,
        },
    )


def build_apply_improvement_prompt(
    finding: Row, worktree: Path, branch: str, repo: Row, repo_notes: str = ""
) -> str:
    subset = _finding_subset(finding)
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "apply_improvement.md").read_text(),
        {
            "WORKTREE": worktree,
            "BRANCH": branch,
            "FINDING_JSON": _escape_braces(json.dumps(subset, indent=2)),
            "REPO_NAME": repo["name"],
            "REPO_NOTES": notes,
        },
    )


def build_apply_modernization_prompt(
    finding: Row, worktree: Path, branch: str, repo: Row, repo_notes: str = ""
) -> str:
    subset = _finding_subset(finding)
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "apply_modernization.md").read_text(),
        {
            "WORKTREE": worktree,
            "BRANCH": branch,
            "FINDING_JSON": _escape_braces(json.dumps(subset, indent=2)),
            "REPO_NAME": repo["name"],
            "REPO_NOTES": notes,
        },
    )


def _feedback_blocks(pr: Row, cap: int = 8000) -> str:
    """Chronological comments + reviews; oldest dropped past ~cap chars."""
    items: list[tuple[str, str, str]] = [
        (
            c.get("createdAt") or "",
            (c.get("author") or {}).get("login") or "?",
            (c.get("body") or "").strip(),
        )
        for c in pr.get("comments") or []
    ]
    for r in pr.get("reviews") or []:
        state, body = r.get("state") or "", (r.get("body") or "").strip()
        if state and state != "COMMENTED":
            body = f"[review: {state}] {body}".strip()
        if body:
            items.append(
                (
                    r.get("submittedAt") or "",
                    (r.get("author") or {}).get("login") or "?",
                    body,
                )
            )
    items.sort()
    blocks = [f"### {who} at {ts}\n{body}" for ts, who, body in items]
    dropped = 0
    while len(blocks) > 1 and sum(len(b) + 2 for b in blocks) > cap:
        blocks.pop(0)
        dropped += 1
    if dropped:
        blocks.insert(0, f"({dropped} older item(s) elided)")
    return "\n\n".join(blocks) or "(no comments or reviews)"


def _checks_lines(rollup: list[Row] | None) -> str:
    if not rollup:
        return "(no checks reported)"
    return "\n".join(
        f"- {c.get('name') or c.get('context') or '?'}: "
        f"{c.get('conclusion') or c.get('state') or 'PENDING'}"
        for c in rollup[:30]
    )
def build_engage_prompt(
    worktree: Path,
    head_ref: str,
    repo: Row,
    pr: Row,
    attention: str,
    repo_notes: str = "",
) -> str:
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "engage.md").read_text(),
        {
            "WORKTREE": worktree,
            "BRANCH": head_ref,
            "REPO_NAME": repo["name"],
            "DEFAULT_BRANCH": repo["default_branch"],
            "PR_TITLE": _escape_braces(pr.get("title") or ""),
            "PR_BODY": _escape_braces(pr.get("body") or "(no description)"),
            "FEEDBACK": _escape_braces(_feedback_blocks(pr)),
            "CHECKS": _escape_braces(_checks_lines(pr.get("statusCheckRollup"))),
            "ATTENTION": attention or "(none recorded)",
            "REPO_NOTES": notes,
        },
    )


def build_harvest_prompt(
    finding: Row,
    worktree: Path,
    repo: Row,
    pr: Row,
    pr_number: int,
    repo_notes: str = "",
) -> str:
    subset = _finding_subset(finding)
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "harvest.md").read_text(),
        {
            "WORKTREE": worktree,
            "REPO_NAME": repo["name"],
            "DEFAULT_BRANCH": repo["default_branch"],
            "FINDING_JSON": _escape_braces(json.dumps(subset, indent=2)),
            "PR_NUMBER": pr_number,
            "PR_TITLE": _escape_braces(pr.get("title") or ""),
            "PR_BODY": _escape_braces(pr.get("body") or "(no description)"),
            "FEEDBACK": _escape_braces(_feedback_blocks(pr)),
            "REPO_NOTES": notes,
        },
    )


def build_recheck_prompt(finding: Row, repo: Row, out_path: Path, repo_notes: str = "") -> str:
    subset = _finding_subset(finding)
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "recheck.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "FINDING_JSON": _escape_braces(json.dumps(subset, indent=2)),
            "OUT_PATH": str(out_path),
            "REPO_NOTES": notes,
        },
    )


def build_test_gap_prompt(
    repo: Row,
    scope_note: str,
    suppressions: list[Row],
    known_gaps: list[Row],
    out_path: Path,
    max_gaps: int,
    repo_notes: str = "",
) -> str:
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "test_gap.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "SCOPE_NOTE": scope_note,
            "SUPPRESSIONS": _suppressions_block(suppressions),
            "KNOWN_GAPS": _known_block(known_gaps),
            "OUT_PATH": out_path,
            "MAX_GAPS": max_gaps,
            "REPO_NOTES": notes,
        },
    )


def build_dep_update_prompt(
    repo: Row,
    scope_note: str,
    suppressions: list[Row],
    known_updates: list[Row],
    out_path: Path,
    max_updates: int,
    repo_notes: str = "",
) -> str:
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "dep_update.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "SCOPE_NOTE": scope_note,
            "SUPPRESSIONS": _suppressions_block(suppressions),
            "KNOWN_UPDATES": _known_block(known_updates),
            "OUT_PATH": out_path,
            "MAX_UPDATES": max_updates,
            "REPO_NOTES": notes,
        },
    )


def build_refactor_prompt(
    repo: Row,
    scope_note: str,
    suppressions: list[Row],
    known_refactors: list[Row],
    out_path: Path,
    max_refactors: int,
    repo_notes: str = "",
) -> str:
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "refactor.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "SCOPE_NOTE": scope_note,
            "SUPPRESSIONS": _suppressions_block(suppressions),
            "KNOWN_REFACTORS": _known_block(known_refactors),
            "OUT_PATH": out_path,
            "MAX_REFACTORS": max_refactors,
            "REPO_NOTES": notes,
        },
    )


def build_modernization_prompt(
    repo: Row,
    scope_note: str,
    suppressions: list[Row],
    known_modernizations: list[Row],
    out_path: Path,
    max_modernizations: int,
    repo_notes: str = "",
) -> str:
    notes = _escape_braces(repo_notes) if repo_notes else "(No notes yet)"
    return _render(
        (PLAYBOOK_DIR / "modernization.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "SCOPE_NOTE": scope_note,
            "SUPPRESSIONS": _suppressions_block(suppressions),
            "KNOWN_MODERNIZATIONS": _known_block(known_modernizations),
            "OUT_PATH": out_path,
            "MAX_MODERNIZATIONS": max_modernizations,
            "REPO_NOTES": notes,
        },
    )
