"""Tests for hunter.scheduler._checks_summary -- the short human-readable
"N pass / N fail / N pending" rollup summary used by sync_prs and the UI.
"""

from __future__ import annotations

from hunter.scheduler import _checks_summary


def test_empty_rollup_returns_none() -> None:
    assert _checks_summary(None) == (None, False, [])
    assert _checks_summary([]) == (None, False, [])


def test_all_passing() -> None:
    rollup = [
        {"name": "CI", "conclusion": "SUCCESS"},
        {"name": "Lint", "conclusion": "NEUTRAL"},
    ]
    summary, failing, names = _checks_summary(rollup)
    assert summary == "2 pass"
    assert failing is False
    assert names == []


def test_mixed_pass_fail_pending() -> None:
    rollup = [
        {"name": "CI", "conclusion": "SUCCESS"},
        {"name": "Lint", "conclusion": "FAILURE"},
        {"name": "Deploy", "conclusion": "PENDING"},
    ]
    summary, failing, names = _checks_summary(rollup)
    assert summary == "1 pass / 1 fail / 1 pending"
    assert failing is True
    assert names == ["Lint"]


def test_duplicate_failing_check_name_does_not_inflate_pending() -> None:
    """Regression: a rollup with the SAME check name failing twice (e.g.
    a matrix job reporting per-shard) used to compute pending from the
    DEDUPLICATED failing-name count, so len(named) - len(failing_names)
    - passing overcounted "pending" by however many duplicate failures
    existed -- a rollup with two FAILURE entries both named 'CI' and
    nothing else reported '0 pass / 1 fail / 1 pending' when nothing was
    actually pending."""
    rollup = [
        {"name": "CI", "conclusion": "FAILURE"},
        {"name": "CI", "conclusion": "FAILURE"},
    ]
    summary, failing, names = _checks_summary(rollup)
    assert summary == "0 pass / 2 fail"
    assert failing is True
    assert names == ["CI"]


def test_duplicate_failing_names_mixed_with_a_genuinely_pending_check() -> None:
    rollup = [
        {"name": "CI", "conclusion": "FAILURE"},
        {"name": "CI", "conclusion": "FAILURE"},
        {"name": "Deploy", "conclusion": "PENDING"},
    ]
    summary, _failing, names = _checks_summary(rollup)
    assert summary == "0 pass / 2 fail / 1 pending"
    assert names == ["CI"]


def test_state_field_used_when_conclusion_absent() -> None:
    """GitLab-shaped rollup entries use 'state' instead of 'conclusion'."""
    rollup = [{"name": "pipeline", "state": "failure"}]
    _summary, failing, names = _checks_summary(rollup)
    assert failing is True
    assert names == ["pipeline"]


def test_missing_name_falls_back_to_context_then_placeholder() -> None:
    rollup = [{"context": "ci/build", "conclusion": "FAILURE"}, {"conclusion": "SUCCESS"}]
    _summary, _failing, names = _checks_summary(rollup)
    assert names == ["ci/build"]
