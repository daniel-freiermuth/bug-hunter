"""Tests for hunter.playbooks."""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter.playbooks import (
    _checks_lines,
    _escape_braces,
    _feedback_blocks,
    _render,
    build_engage_prompt,
    build_fix_prompt,
    build_hunt_prompt,
    build_recheck_prompt,
)


def _repo() -> dict:
    return {"path": "/tmp/repo", "name": "test", "default_branch": "main"}


def _finding() -> dict:
    return {
        "fingerprint": "abc123",
        "file": "src/main.py",
        "symbol": "do_stuff",
        "line": 42,
        "bug_class": "logic",
        "severity": "medium",
        "confidence": "high",
        "summary": "Off-by-one in loop",
        "detail": "The loop iterates one too many times.",
        "evidence_plan": "Add a test with boundary input.",
        "introduced_by": "deadbeef",
    }


# -- _render -----------------------------------------------------------------


class TestRender:
    def test_substitutes_all_keys(self):
        tpl = "Hello {{NAME}}, you have {{COUNT}} items."
        result = _render(tpl, {"NAME": "Alice", "COUNT": 3})
        assert result == "Hello Alice, you have 3 items."

    def test_no_placeholders_left(self):
        tpl = "{{A}} and {{B}}"
        result = _render(tpl, {"A": "x", "B": "y"})
        assert "{{" not in result

    def test_unfilled_placeholder_raises(self):
        tpl = "{{A}} and {{B}}"
        with pytest.raises(AssertionError, match="unfilled placeholder"):
            _render(tpl, {"A": "x"})

    def test_value_coerced_to_str(self):
        tpl = "path={{P}}"
        result = _render(tpl, {"P": Path("/tmp/foo")})
        assert result == "path=/tmp/foo"


# -- _escape_braces ----------------------------------------------------------


class TestEscapeBraces:
    def test_double_braces_escaped(self):
        assert _escape_braces("{{hello}}") == "{ {hello}}"

    def test_no_braces_unchanged(self):
        assert _escape_braces("plain text") == "plain text"

    def test_single_brace_unchanged(self):
        assert _escape_braces("{single}") == "{single}"

    def test_non_string_coerced(self):
        assert _escape_braces(42) == "42"


# -- _feedback_blocks --------------------------------------------------------


class TestFeedbackBlocks:
    def test_empty_pr(self):
        assert _feedback_blocks({}) == "(no comments or reviews)"

    def test_no_comments_no_reviews(self):
        pr: dict = {"comments": [], "reviews": []}
        assert _feedback_blocks(pr) == "(no comments or reviews)"

    def test_comments_and_reviews_chronological(self):
        pr: dict = {
            "comments": [
                {
                    "createdAt": "2026-01-02T00:00:00Z",
                    "author": {"login": "bob"},
                    "body": "looks good",
                },
                {
                    "createdAt": "2026-01-01T00:00:00Z",
                    "author": {"login": "alice"},
                    "body": "first comment",
                },
            ],
            "reviews": [
                {
                    "submittedAt": "2026-01-03T00:00:00Z",
                    "author": {"login": "carol"},
                    "state": "APPROVED",
                    "body": "Ship it!",
                },
            ],
        }
        result = _feedback_blocks(pr)
        # alice (01-01) before bob (01-02) before carol (01-03)
        alice_pos = result.index("alice")
        bob_pos = result.index("bob")
        carol_pos = result.index("carol")
        assert alice_pos < bob_pos < carol_pos
        assert "[review: APPROVED]" in result

    def test_review_commented_state_not_prefixed(self):
        pr: dict = {
            "reviews": [
                {
                    "submittedAt": "2026-01-01T00:00:00Z",
                    "author": {"login": "dan"},
                    "state": "COMMENTED",
                    "body": "nitpick",
                },
            ],
        }
        result = _feedback_blocks(pr)
        assert "[review:" not in result
        assert "nitpick" in result

    def test_cap_drops_oldest(self):
        comments = [
            {
                "createdAt": f"2026-01-{i:02d}T00:00:00Z",
                "author": {"login": f"user{i}"},
                "body": "x" * 500,
            }
            for i in range(1, 30)
        ]
        pr: dict = {"comments": comments}
        result = _feedback_blocks(pr, cap=2000)
        assert "elided" in result
        # The latest comment should survive
        assert "user29" in result


# -- _checks_lines -----------------------------------------------------------


class TestChecksLines:
    def test_none_input(self):
        assert _checks_lines(None) == "(no checks reported)"

    def test_empty_list(self):
        assert _checks_lines([]) == "(no checks reported)"

    def test_formatted_lines(self):
        checks = [
            {"name": "lint", "conclusion": "SUCCESS"},
            {"context": "ci/build", "state": "FAILURE"},
        ]
        result = _checks_lines(checks)
        assert "- lint: SUCCESS" in result
        assert "- ci/build: FAILURE" in result

    def test_missing_fields_fallback(self):
        result = _checks_lines([{}])
        assert "- ?: PENDING" in result


# -- build_hunt_prompt -------------------------------------------------------


class TestBuildHuntPrompt:
    def test_returns_nonempty_string(self):
        result = build_hunt_prompt(
            repo=_repo(),
            diff_range="abc..def",
            scope_note="focus on src/",
            suppressions=[],
            known=[],
            out_path=Path("/tmp/findings.json"),
            max_findings=10,
        )
        assert isinstance(result, str)
        assert len(result) > 0
        assert "{{" not in result

    def test_with_suppressions_and_known(self):
        result = build_hunt_prompt(
            repo=_repo(),
            diff_range="abc..def",
            scope_note="",
            suppressions=[
                {"fingerprint": "fp1", "verdict_reason": "not a bug"},
            ],
            known=[
                {"fingerprint": "fp2", "status": "new", "summary": "thing"},
            ],
            out_path=Path("/tmp/out.json"),
            max_findings=5,
        )
        assert "fp1" in result
        assert "fp2" in result


# -- build_fix_prompt --------------------------------------------------------


class TestBuildFixPrompt:
    def test_returns_nonempty_string(self):
        result = build_fix_prompt(
            finding=_finding(),
            worktree=Path("/tmp/wt"),
            branch="fix/abc123",
            repo=_repo(),
        )
        assert isinstance(result, str)
        assert len(result) > 0
        assert "{{" not in result


# -- build_engage_prompt -----------------------------------------------------


class TestBuildEngagePrompt:
    def test_returns_nonempty_string(self):
        pr: dict = {
            "title": "Fix bug",
            "body": "Fixes the off-by-one.",
            "comments": [],
            "reviews": [],
            "statusCheckRollup": [],
        }
        result = build_engage_prompt(
            worktree=Path("/tmp/wt"),
            head_ref="fix/abc",
            repo=_repo(),
            pr=pr,
            attention="reviewer asked for changes",
        )
        assert isinstance(result, str)
        assert len(result) > 0
        assert "{{" not in result

    def test_braces_in_pr_title_escaped(self):
        pr: dict = {
            "title": "Fix {{template}} issue",
            "body": "",
            "comments": [],
            "reviews": [],
        }
        result = build_engage_prompt(
            worktree=Path("/tmp/wt"),
            head_ref="fix/abc",
            repo=_repo(),
            pr=pr,
            attention="",
        )
        # Should not raise on unfilled placeholder
        assert isinstance(result, str)


# -- build_recheck_prompt ----------------------------------------------------


class TestBuildRecheckPrompt:
    def test_returns_nonempty_string(self):
        result = build_recheck_prompt(
            finding=_finding(),
            repo=_repo(),
            out_path=Path("/tmp/recheck.json"),
        )
        assert isinstance(result, str)
        assert len(result) > 0
        assert "{{" not in result
