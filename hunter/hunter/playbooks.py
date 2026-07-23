"""Render worker prompts from playbook templates."""
from __future__ import annotations

import json
from pathlib import Path

from .types import PLAYBOOK_DIR

_FINDING_PROMPT_KEYS = (
    "fingerprint", "file", "symbol", "line", "bug_class", "severity",
    "confidence", "summary", "detail", "evidence_plan", "introduced_by",
)


def _render(template: str, slots: dict) -> str:
    for k, v in slots.items():
        template = template.replace("{{" + k + "}}", str(v))
    assert "{{" not in template, f"unfilled placeholder in playbook: {template[template.index('{{'):][:60]}"
    return template


def build_hunt_prompt(repo: dict, diff_range: str, scope_note: str,
                      suppressions: list[dict], known: list[dict],
                      out_path: Path, max_findings: int) -> str:
    sup = "\n".join(
        f"- {s['fingerprint']} — {s.get('verdict_reason') or '(no reason recorded)'}"
        for s in suppressions
    ) or "(none yet)"
    kn = "\n".join(
        f"- {k['fingerprint']} [{k['status']}] — {k.get('summary', '')}"
        for k in known
    ) or "(none yet)"
    return _render((PLAYBOOK_DIR / "hunt.md").read_text(), {
        "REPO_PATH": repo["path"],
        "REPO_NAME": repo["name"],
        "DIFF_RANGE": diff_range,
        "SCOPE_NOTE": scope_note,
        "SUPPRESSIONS": sup,
        "KNOWN_FINDINGS": kn,
        "OUT_PATH": out_path,
        "MAX_FINDINGS": max_findings,
    })


def build_fix_prompt(finding: dict, worktree: Path, branch: str, repo: dict) -> str:
    subset = {k: finding.get(k) for k in _FINDING_PROMPT_KEYS}
    return _render((PLAYBOOK_DIR / "fix.md").read_text(), {
        "WORKTREE": worktree,
        "BRANCH": branch,
        "FINDING_JSON": json.dumps(subset, indent=2),
        "REPO_NAME": repo["name"],
    })
