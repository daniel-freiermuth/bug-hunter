"""Shared utilities -- subprocess wrapper used by forge and scheduler."""

from __future__ import annotations

import subprocess


def run_cmd(
    cmd: list[str],
    cwd: str | None = None,
    timeout: int = 300,
) -> tuple[int, str]:
    """Run a command; return (rc, combined stdout+stderr stripped).

    Never raises — all errors map to an rc + message pair.
    """
    try:
        p = subprocess.run(
            cmd,
            cwd=cwd,
            capture_output=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=timeout,
            check=False,
        )
        return p.returncode, (p.stdout or "").strip()
    except subprocess.TimeoutExpired:
        return 124, f"timeout after {timeout}s: {' '.join(map(str, cmd))}"
    except OSError as e:
        return 127, str(e)
