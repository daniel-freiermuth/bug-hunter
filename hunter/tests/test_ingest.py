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
