"""Render worker prompts from playbook templates."""

from __future__ import annotations

import json
from pathlib import Path

from .types import PLAYBOOK_DIR, Row

_FINDING_PROMPT_KEYS = (
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


def build_hunt_prompt(
    repo: Row,
    diff_range: str,
    scope_note: str,
    suppressions: list[Row],
    known: list[Row],
    out_path: Path,
    max_findings: int,
) -> str:
    sup = (
        "\n".join(
            f"- {s['fingerprint']} -- {s.get('verdict_reason') or '(no reason recorded)'}"
            for s in suppressions
        )
        or "(none yet)"
    )
    kn = (
        "\n".join(f"- {k['fingerprint']} [{k['status']}] -- {k.get('summary', '')}" for k in known)
        or "(none yet)"
    )
    return _render(
        (PLAYBOOK_DIR / "hunt.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "DIFF_RANGE": diff_range,
            "SCOPE_NOTE": scope_note,
            "SUPPRESSIONS": sup,
            "KNOWN_FINDINGS": kn,
            "OUT_PATH": out_path,
            "MAX_FINDINGS": max_findings,
        },
    )


def build_fix_prompt(finding: Row, worktree: Path, branch: str, repo: Row) -> str:
    subset = {k: finding.get(k) for k in _FINDING_PROMPT_KEYS}
    return _render(
        (PLAYBOOK_DIR / "fix.md").read_text(),
        {
            "WORKTREE": worktree,
            "BRANCH": branch,
            "FINDING_JSON": json.dumps(subset, indent=2),
            "REPO_NAME": repo["name"],
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
) -> str:
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
        },
    )


def build_recheck_prompt(finding: Row, repo: Row, out_path: Path) -> str:
    subset = {k: finding.get(k) for k in _FINDING_PROMPT_KEYS}
    return _render(
        (PLAYBOOK_DIR / "recheck.md").read_text(),
        {
            "REPO_PATH": repo["path"],
            "REPO_NAME": repo["name"],
            "FINDING_JSON": json.dumps(subset, indent=2),
            "OUT_PATH": str(out_path),
        },
    )
