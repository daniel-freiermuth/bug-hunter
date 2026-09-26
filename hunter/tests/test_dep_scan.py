"""dep_scan.scan_repo: which renovate runs, and with what environment.

Parity with hunter-rs tests/dep_scan_test.rs. The scanned checkout is
untrusted content: nothing it ships may be executed, and the daemon's
secrets must not reach the child.
"""

from __future__ import annotations

import os
import stat
from typing import TYPE_CHECKING

from hunter.dep_scan import scan_repo

if TYPE_CHECKING:
    from pathlib import Path

    import pytest

NO_UPDATE_LINE = (
    '{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":'
    '[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0",'
    '"datasource":"npm","updates":[]}]}]}}'
)


def _exe(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\n" + body + "\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _repo_with_evil(tmp_path: Path, name: str) -> tuple[Path, Path]:
    repo = tmp_path / "repo"
    marker = repo / "HIJACKED"
    _exe(repo / "node_modules" / ".bin" / name, f"touch '{marker}'")
    return repo, marker


def test_a_renovate_shipped_by_the_scanned_repo_never_runs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo, marker = _repo_with_evil(tmp_path, "renovate")
    installed = tmp_path / "bin"
    _exe(installed / "renovate", f"echo '{NO_UPDATE_LINE}'")
    repo_bin = repo / "node_modules" / ".bin"
    hostile = os.pathsep.join(
        ["node_modules/.bin", str(repo_bin), str(installed), "/usr/bin", "/bin"]
    )
    monkeypatch.setenv("PATH", hostile)

    got = scan_repo(repo, "acme/widget", timeout=30)

    assert not marker.exists(), "the scanned repo's own renovate ran"
    assert got == []


def test_a_repo_shipped_renovate_is_not_a_fallback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo, marker = _repo_with_evil(tmp_path, "renovate")
    monkeypatch.setenv("PATH", str(repo / "node_modules" / ".bin"))

    assert scan_repo(repo, "acme/widget", timeout=30) is None
    assert not marker.exists()


def test_renovates_own_path_lookups_cannot_reach_the_checkout(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo, marker = _repo_with_evil(tmp_path, "node")
    installed = tmp_path / "bin"
    _exe(installed / "renovate", f"node --version >/dev/null 2>&1\necho '{NO_UPDATE_LINE}'")
    hostile = os.pathsep.join(["node_modules/.bin", str(installed), "/usr/bin", "/bin"])
    monkeypatch.setenv("PATH", hostile)

    assert scan_repo(repo, "acme/widget", timeout=30) == []
    assert not marker.exists(), "renovate's PATH resolved into the checkout"


def test_renovate_does_not_inherit_the_daemons_secrets(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    seen = tmp_path / "env.txt"
    installed = tmp_path / "bin"
    _exe(installed / "renovate", f"env > '{seen}'\necho '{NO_UPDATE_LINE}'")
    monkeypatch.setenv("PATH", os.pathsep.join([str(installed), "/usr/bin", "/bin"]))
    monkeypatch.setenv("GH_TOKEN", "s3cret-token")
    monkeypatch.setenv("HOME", str(tmp_path))

    assert scan_repo(repo, "acme/widget", timeout=30) == []
    env = seen.read_text()
    assert "s3cret-token" not in env
    assert any(line.startswith("HOME=") for line in env.splitlines())
    assert "LOG_FORMAT=json" in env
