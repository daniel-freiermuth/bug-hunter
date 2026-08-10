"""Tests for hunter.ingest.ingest_findings."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from hunter.ingest import ingest_findings
from hunter.store import Store
from hunter.types import Config


def _make_finding(**overrides: object) -> dict[str, object]:
    base: dict[str, object] = {
        "fingerprint": "fp-001",
        "bug_class": "logic",
        "severity": "high",
        "confidence": 0.9,
        "summary": "test finding",
        "file": "foo.py",
    }
    base.update(overrides)
    return base


@pytest.fixture
def env(tmp_path: Path) -> tuple[Store, int, Path]:
    """Return (store, repo_id, findings_dir)."""
    cfg = Config(work_root=tmp_path, db_path=tmp_path / "test.db")
    store = Store(cfg)
    repo_id = store.add_repo("test", "https://example.com/test", "/tmp/repo")
    findings_dir = tmp_path / "findings"
    findings_dir.mkdir()
    return store, repo_id, findings_dir


def _write_findings(directory: Path, entries: object) -> Path:
    p = directory / "findings.json"
    p.write_text(json.dumps(entries))
    return p


# ── valid findings → inserted count ──────────────────────────────────


def test_valid_findings_inserted(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        _make_finding(fingerprint="a"),
        _make_finding(fingerprint="b"),
        _make_finding(fingerprint="c"),
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result == {"inserted": 3, "duplicates": 0, "invalid": 0}


# ── duplicate fingerprints ───────────────────────────────────────────


def test_duplicate_fingerprints(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        _make_finding(fingerprint="dup"),
        _make_finding(fingerprint="dup"),
        _make_finding(fingerprint="unique"),
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result == {"inserted": 2, "duplicates": 1, "invalid": 0}


def test_duplicate_across_calls(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    p = _write_findings(fdir, [_make_finding(fingerprint="x")])
    r1 = ingest_findings(store, repo_id, p)
    assert r1["inserted"] == 1

    p2 = fdir / "findings2.json"
    p2.write_text(json.dumps([_make_finding(fingerprint="x")]))
    r2 = ingest_findings(store, repo_id, p2)
    assert r2 == {"inserted": 0, "duplicates": 1, "invalid": 0}


# ── invalid entries ──────────────────────────────────────────────────


def test_missing_fingerprint(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        _make_finding(fingerprint="ok"),
        _make_finding(fingerprint=""),  # empty → falsy
        _make_finding(fingerprint="ok2"),
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result["invalid"] == 1
    assert result["inserted"] == 2


def test_bad_bug_class(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [_make_finding(bug_class="nonexistent")]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


def test_bad_severity(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [_make_finding(severity="critical")]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


def test_non_numeric_confidence(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [_make_finding(confidence="not-a-number")]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


def test_invalid_mixed_with_valid(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        _make_finding(fingerprint="good1"),
        _make_finding(bug_class="bad"),  # invalid
        _make_finding(fingerprint="good2"),
        "not a dict",  # invalid
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries))
    assert result == {"inserted": 2, "duplicates": 0, "invalid": 2}


# ── non-list JSON root ──────────────────────────────────────────────


def test_non_list_json_root(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    result = ingest_findings(store, repo_id, _write_findings(fdir, {"key": "val"}))
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


def test_json_root_string(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    result = ingest_findings(store, repo_id, _write_findings(fdir, "just a string"))
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


# ── unreadable file ──────────────────────────────────────────────────


def test_unreadable_file(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    result = ingest_findings(store, repo_id, fdir / "no_such_file.json")
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


def test_malformed_json(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    p = fdir / "bad.json"
    p.write_text("{not valid json")
    result = ingest_findings(store, repo_id, p)
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 1}


# ── confidence clamping ──────────────────────────────────────────────


def test_confidence_clamped_high(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [_make_finding(fingerprint="clamp-hi", confidence=5.0)]
    ingest_findings(store, repo_id, _write_findings(fdir, entries))
    row = store.get_finding(1)
    assert row is not None
    assert row["confidence"] == 1.0


def test_confidence_clamped_low(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [_make_finding(fingerprint="clamp-lo", confidence=-0.5)]
    ingest_findings(store, repo_id, _write_findings(fdir, entries))
    row = store.get_finding(1)
    assert row is not None
    assert row["confidence"] == 0.0


def test_confidence_in_range_unchanged(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [_make_finding(fingerprint="mid", confidence=0.42)]
    ingest_findings(store, repo_id, _write_findings(fdir, entries))
    row = store.get_finding(1)
    assert row is not None
    assert row["confidence"] == pytest.approx(0.42)


# ── empty list ───────────────────────────────────────────────────────


def test_empty_list(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    result = ingest_findings(store, repo_id, _write_findings(fdir, []))
    assert result == {"inserted": 0, "duplicates": 0, "invalid": 0}


# ── non-bug finding types (unified findings table) ─────────────────────


def test_dep_update_ingested_with_type_columns(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        {
            "fingerprint": "repo:npm:foo:1.0.0->2.0.0",
            "ecosystem": "npm",
            "package": "foo",
            "current_version": "1.0.0",
            "latest_version": "2.0.0",
            "update_type": "major",
            "severity": "medium",
            "confidence": 0.8,
            "summary": "foo 2.0.0 breaking changes",
        }
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries), finding_type="dep_update")
    assert result == {"inserted": 1, "duplicates": 0, "invalid": 0}
    rows = store.list_all_findings(finding_type="dep_update")
    assert len(rows) == 1
    assert rows[0]["type"] == "dep_update"
    assert rows[0]["package"] == "foo"
    assert rows[0]["current_version"] == "1.0.0"
    assert rows[0]["category"] == "major"
    # bug_class must stay unset for non-bug findings
    assert rows[0]["bug_class"] is None


def test_dep_update_missing_bug_class_is_not_invalid(env: tuple[Store, int, Path]) -> None:
    """dep_update/test_gap/refactor findings never carry bug_class -- only
    bug findings require it."""
    store, repo_id, fdir = env
    entries = [{"fingerprint": "repo:npm:foo:1.0.0->2.0.0", "package": "foo"}]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries), finding_type="dep_update")
    assert result == {"inserted": 1, "duplicates": 0, "invalid": 0}


def test_test_gap_ingested_with_missing_tests_list(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        {
            "fingerprint": "repo:foo.py:bar:test-gap",
            "file": "foo.py",
            "symbol": "bar",
            "severity": "medium",
            "confidence": 0.7,
            "summary": "bar has no test for empty input",
            "missing_tests": ["empty input", "negative numbers"],
        }
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries), finding_type="test_gap")
    assert result == {"inserted": 1, "duplicates": 0, "invalid": 0}
    rows = store.list_all_findings(finding_type="test_gap")
    assert rows[0]["type"] == "test_gap"
    assert rows[0]["category"] == "coverage"
    assert json.loads(rows[0]["missing_tests"]) == ["empty input", "negative numbers"]


def test_refactor_ingested_with_type_columns(env: tuple[Store, int, Path]) -> None:
    store, repo_id, fdir = env
    entries = [
        {
            "fingerprint": "repo:foo.py:bar:duplication",
            "file": "foo.py",
            "symbol": "bar",
            "smell_type": "duplication",
            "severity": "low",
            "confidence": 0.6,
            "summary": "bar duplicates baz",
            "suggested_refactor": "extract shared helper",
        }
    ]
    result = ingest_findings(store, repo_id, _write_findings(fdir, entries), finding_type="refactor")
    assert result == {"inserted": 1, "duplicates": 0, "invalid": 0}
    rows = store.list_all_findings(finding_type="refactor")
    assert rows[0]["type"] == "refactor"
    assert rows[0]["category"] == "duplication"
    assert rows[0]["suggested_refactor"] == "extract shared helper"


def test_different_types_do_not_leak_into_each_others_known_list(
    env: tuple[Store, int, Path],
) -> None:
    """A bug and a dep_update finding sharing a repo must not cross-contaminate
    type-filtered queries -- this was the root cause of dep_update hunts
    re-filing already-rejected findings."""
    store, repo_id, fdir = env
    ingest_findings(store, repo_id, _write_findings(fdir, [_make_finding()]), finding_type="bug")
    ingest_findings(
        store,
        repo_id,
        _write_findings(fdir, [{"fingerprint": "repo:npm:foo:1.0.0->2.0.0", "package": "foo"}]),
        finding_type="dep_update",
    )
    assert len(store.list_all_findings(finding_type="bug")) == 1
    assert len(store.list_all_findings(finding_type="dep_update")) == 1
    assert len(store.list_all_findings()) == 2
