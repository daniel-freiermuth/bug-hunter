"""Static dependency scanner using Renovate's local platform.

Runs an installed `renovate --platform=local --dry-run=lookup` against a repo
clone and parses the JSON log output into structured update candidates
compatible with the findings ingest format.

Zero AI tokens — Renovate handles ecosystem detection, registry queries,
semver classification, and advisory lookup.
"""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any

from .util import run_cmd

log = logging.getLogger(__name__)


# The only variables Renovate's child sees, besides the sanitised PATH and
# the two logging switches. Mirrors hunter-rs dep_scan.rs FORWARDED_ENV.
_FORWARDED_ENV = (
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
)


def _resolve_renovate(repo_path: Path) -> tuple[str, str] | None:
    """The operator's renovate, and the PATH to run it with.

    Not `npx renovate`: npx resolves from the current directory's
    node_modules/.bin first, and the current directory is the checkout being
    scanned, so a repo shipping node_modules/.bin/renovate had its own
    program run as the daemon; with nothing installed, npx downloaded
    whatever the registry served. Only absolute PATH entries outside the
    checkout are searched, the same filtered PATH is given to the child (so
    renovate's `#!/usr/bin/env node` cannot resolve into the repo either),
    and nothing is ever downloaded. Mirrors hunter-rs dep_scan.rs.
    """
    try:
        repo = repo_path.resolve(strict=True)
    except OSError:
        return None

    def inside_repo(p: Path) -> bool:
        try:
            return p.resolve(strict=True).is_relative_to(repo)
        except OSError:
            return False

    dirs = [
        Path(d)
        for d in os.environ.get("PATH", "").split(os.pathsep)
        if d and Path(d).is_absolute() and not inside_repo(Path(d))
    ]
    for d in dirs:
        candidate = d / "renovate"
        if candidate.is_file() and os.access(candidate, os.X_OK) and not inside_repo(candidate):
            return str(candidate), os.pathsep.join(str(x) for x in dirs)
    return None


def scan_repo(repo_path: Path, repo_name: str, timeout: int = 120) -> list[dict[str, Any]] | None:
    """Run Renovate in local dry-run mode and return update candidates.

    Each candidate is a dict matching the dep_update finding schema:
    fingerprint, ecosystem, package, current_version, latest_version,
    update_type, severity, confidence, summary, detail.

    Returns [] on failure (Renovate not installed, timeout, parse error).
    """
    resolved = _resolve_renovate(repo_path)
    if resolved is None:
        log.info("dep_scan: no installed renovate usable for %s", repo_name)
        return None
    renovate, clean_path = resolved
    env = {k: os.environ[k] for k in _FORWARDED_ENV if k in os.environ}
    env |= {"PATH": clean_path, "LOG_FORMAT": "json", "LOG_LEVEL": "debug"}
    rc, output = run_cmd(
        [renovate, "--platform=local", "--dry-run=lookup"],
        cwd=str(repo_path),
        timeout=timeout,
        env=env,
    )

    if rc != 0 and not output:
        log.warning("dep_scan: renovate failed (rc=%d) for %s", rc, repo_name)
        return None

    updates = _parse_renovate_output(output, repo_name)
    log.info("dep_scan: %s — %d update candidates from renovate", repo_name, len(updates))
    return updates


def _parse_renovate_output(output: str, repo_name: str) -> list[dict[str, Any]]:
    """Extract update candidates from Renovate's JSON log lines."""
    candidates: list[dict[str, Any]] = []
    seen: set[str] = set()

    for line in output.splitlines():
        try:
            obj = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue

        config = obj.get("config")
        if not isinstance(config, dict):
            continue

        for manager, files in config.items():
            if not isinstance(files, list):
                continue
            for pf in files:
                if not isinstance(pf, dict):
                    continue
                for dep in pf.get("deps", []):
                    dep_name = dep.get("depName") or dep.get("packageName")
                    if not dep_name:
                        continue
                    current = dep.get("currentValue") or dep.get("currentVersion") or "?"
                    datasource = dep.get("datasource") or manager

                    for u in dep.get("updates", []):
                        new_version = u.get("newVersion") or u.get("newValue") or "?"
                        update_type = u.get("updateType") or "unknown"

                        fp = f"{repo_name}:{datasource}:{dep_name}:{current}\u2192{new_version}"
                        if fp in seen:
                            continue
                        seen.add(fp)

                        severity = _severity(update_type, u)
                        confidence = _confidence(update_type)

                        candidates.append(
                            {
                                "fingerprint": fp,
                                "file": pf.get("packageFile") or "",
                                "ecosystem": datasource,
                                "package": dep_name,
                                "current_version": str(current).lstrip("^~>=<"),
                                "latest_version": new_version,
                                "update_type": _normalize_update_type(update_type),
                                "severity": severity,
                                "confidence": confidence,
                                "summary": _summary(dep_name, current, new_version, update_type),
                                "detail": _detail(
                                    dep_name, current, new_version, update_type, manager
                                ),
                            }
                        )

    return candidates


def _normalize_update_type(ut: str) -> str:
    """Map Renovate's updateType to our schema's update_type."""
    if ut == "major":
        return "major"
    if ut == "minor":
        return "minor"
    if ut in ("patch", "pin", "digest", "pinDigest", "lockFileMaintenance"):
        return "patch"
    return ut


def _severity(update_type: str, update: dict[str, Any]) -> str:
    """Determine severity based on update type and advisories."""
    # Renovate flags security updates via isVulnerabilityAlert or similar
    if update.get("isVulnerabilityAlert"):
        return "high"
    if update_type == "major":
        return "medium"
    return "low"


def _confidence(update_type: str) -> float:
    """Confidence that this update is actionable."""
    if update_type == "patch":
        return 0.9
    if update_type == "minor":
        return 0.8
    if update_type == "major":
        return 0.6
    return 0.7


def _summary(name: str, current: str, new: str, update_type: str) -> str:
    return f"{name}: {update_type} update {current} \u2192 {new}"


def _detail(name: str, current: str, new: str, update_type: str, manager: str) -> str:
    lines = [
        f"Package: {name}",
        f"Manager: {manager}",
        f"Current: {current}",
        f"Available: {new}",
        f"Type: {update_type}",
    ]
    if update_type == "major":
        lines.append(
            "Note: major update — may contain breaking changes. Review changelog before applying."
        )
    return "\n".join(lines)
