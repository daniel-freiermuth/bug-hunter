"""Forge abstraction -- GitHub / GitLab PR/MR lifecycle.

Each forge wraps its platform's CLI (gh / glab) and normalises PR/MR
data into a common shape so the scheduler can stay forge-agnostic.
"""

from __future__ import annotations

import contextlib
import json
import re
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .util import run_cmd

if TYPE_CHECKING:
    from .types import Row

# ---------------------------------------------------------------------------
# Base class
# ---------------------------------------------------------------------------


class Forge:
    """PR/MR lifecycle for a specific forge type + host."""

    name: str = "unknown"

    def ssh_url(self, https_url: str) -> str:
        """Convert HTTPS clone URL to an SSH push URL (passthrough default)."""
        return https_url

    def owner_repo(self, url: str) -> str | None:
        """Extract an owner/repo (or group/project) slug from a clone URL."""
        return None

    def parse_pr_url(self, url: str) -> tuple[str, int] | None:
        """Extract (slug, number) from a PR/MR web URL."""
        return None

    # -- lifecycle ----------------------------------------------------------

    def create_pr(
        self,
        slug: str,
        branch: str,
        title: str,
        body_file: Path,
        cwd: str | None = None,
        timeout: int = 300,
    ) -> tuple[int, str]:
        """Create a draft PR/MR.  Returns (rc, pr_url | error_text)."""
        raise NotImplementedError

    def view_pr_sync(
        self, slug: str, number: int, timeout: int = 30
    ) -> tuple[int, Row | None, str]:
        """Fetch PR/MR data for sync_prs.

        Returns (rc, normalised_dict | None, raw_output).
        Normalised keys: state, mergedAt, mergeable, reviewDecision,
        statusCheckRollup, comments, reviews, updatedAt, headRefName.
        """
        raise NotImplementedError

    def view_pr_engage(
        self, slug: str, number: int, timeout: int = 30
    ) -> tuple[int, Row | None, str]:
        """Fetch PR/MR data for run_engage.

        Normalised keys: title, body, comments, reviews, statusCheckRollup.
        """
        raise NotImplementedError

    def comment_pr(
        self, slug: str, number: int, body_file: Path, timeout: int = 60
    ) -> tuple[int, str]:
        """Post a comment on a PR/MR.  Returns (rc, output)."""
        raise NotImplementedError

    def close_pr(self, slug: str, number: int, comment: str, timeout: int = 60) -> tuple[int, str]:
        """Close a PR/MR with a comment.  Returns (rc, output)."""
        raise NotImplementedError


# ---------------------------------------------------------------------------
# GitHub (via `gh` CLI)
# ---------------------------------------------------------------------------


class GitHubForge(Forge):
    name = "github"

    _URL_RE = re.compile(
        r"^(?:https://github\.com/|git@github\.com:)"
        r"([^/]+)/(.+?)(?:\.git)?/?$"
    )
    _PR_RE = re.compile(r"^https://github\.com/([^/]+/[^/]+)/pull/(\d+)")

    def ssh_url(self, https_url: str) -> str:
        m = re.match(r"^https://github\.com/([^/]+)/(.+?)(?:\.git)?/?$", https_url)
        if m:
            return f"git@github.com:{m.group(1)}/{m.group(2)}.git"
        return https_url

    def owner_repo(self, url: str) -> str | None:
        m = self._URL_RE.match(url)
        return f"{m.group(1)}/{m.group(2)}" if m else None

    def parse_pr_url(self, url: str) -> tuple[str, int] | None:
        m = self._PR_RE.match(url)
        return (m.group(1), int(m.group(2))) if m else None

    # -- lifecycle ----------------------------------------------------------

    def create_pr(
        self,
        slug: str,
        branch: str,
        title: str,
        body_file: Path,
        cwd: str | None = None,
        timeout: int = 300,
    ) -> tuple[int, str]:
        cmd = [
            "gh",
            "pr",
            "create",
            "--draft",
            "--head",
            branch,
            "--title",
            title or branch,
            "--body-file",
            str(body_file),
        ]
        if slug:
            cmd[3:3] = ["-R", slug]
        rc, out = run_cmd(cmd, cwd=cwd, timeout=timeout)
        if rc == 0:
            return 0, out.strip().splitlines()[-1] if out else ""
        return rc, out

    def view_pr_sync(
        self, slug: str, number: int, timeout: int = 30
    ) -> tuple[int, Row | None, str]:
        fields = (
            "state,mergedAt,mergeable,reviewDecision,"
            "statusCheckRollup,comments,reviews,updatedAt,headRefName"
        )
        rc, out = run_cmd(
            ["gh", "pr", "view", str(number), "-R", slug, "--json", fields],
            timeout=timeout,
        )
        if rc != 0:
            return rc, None, out
        try:
            return 0, json.loads(out), out
        except ValueError:
            return 1, None, out

    def view_pr_engage(
        self, slug: str, number: int, timeout: int = 30
    ) -> tuple[int, Row | None, str]:
        rc, out = run_cmd(
            [
                "gh",
                "pr",
                "view",
                str(number),
                "-R",
                slug,
                "--json",
                "title,body,comments,reviews,statusCheckRollup",
            ],
            timeout=timeout,
        )
        if rc != 0:
            return rc, None, out
        try:
            return 0, json.loads(out), out
        except ValueError:
            return 1, None, out

    def comment_pr(
        self, slug: str, number: int, body_file: Path, timeout: int = 60
    ) -> tuple[int, str]:
        return run_cmd(
            [
                "gh",
                "pr",
                "comment",
                str(number),
                "-R",
                slug,
                "--body-file",
                str(body_file),
            ],
            timeout=timeout,
        )

    def close_pr(self, slug: str, number: int, comment: str, timeout: int = 60) -> tuple[int, str]:
        return run_cmd(
            [
                "gh",
                "pr",
                "close",
                str(number),
                "-R",
                slug,
                "--comment",
                comment[:800],
            ],
            timeout=timeout,
        )


# ---------------------------------------------------------------------------
# GitLab (via `glab` CLI + REST API)
# ---------------------------------------------------------------------------


class GitLabForge(Forge):
    name = "gitlab"

    def __init__(self, host: str = "gitlab.com") -> None:
        self.host = host
        self._self_hosted = host != "gitlab.com"

    def _host_re(self) -> str:
        return re.escape(self.host)

    # -- URL helpers --------------------------------------------------------

    def ssh_url(self, https_url: str) -> str:
        m = re.match(rf"^https://{self._host_re()}/(.+?)(?:\.git)?/?$", https_url)
        if m:
            return f"git@{self.host}:{m.group(1)}.git"
        return https_url

    def owner_repo(self, url: str) -> str | None:
        m = re.match(
            rf"^(?:https://{self._host_re()}/|git@{self._host_re()}:)"
            r"(.+?)(?:\.git)?/?$",
            url,
        )
        return m.group(1) if m else None

    def parse_pr_url(self, url: str) -> tuple[str, int] | None:
        m = re.match(
            rf"^https://{self._host_re()}/(.+?)/-/merge_requests/(\d+)",
            url,
        )
        return (m.group(1), int(m.group(2))) if m else None

    # -- glab plumbing ------------------------------------------------------

    def _repo_flag(self, slug: str) -> str:
        """Value for glab's ``-R`` flag.

        Self-hosted instances need the full URL so glab resolves the host.
        """
        if self._self_hosted:
            return f"https://{self.host}/{slug}"
        return slug

    def _api(
        self,
        path: str,
        timeout: int = 30,
        method: str | None = None,
        fields: dict[str, str] | None = None,
    ) -> tuple[int, str]:
        """Call ``glab api`` with the right ``--hostname``."""
        cmd = ["glab", "api", path]
        if method:
            cmd += ["--method", method]
        if fields:
            for k, v in fields.items():
                cmd += ["-f", f"{k}={v}"]
        if self._self_hosted:
            cmd += ["--hostname", self.host]
        return run_cmd(cmd, timeout=timeout)

    def _encoded(self, slug: str) -> str:
        return slug.replace("/", "%2F")

    # -- normalisation helpers ----------------------------------------------

    @staticmethod
    def _norm_state(raw: str) -> str:
        s = (raw or "").lower()
        if s == "merged":
            return "MERGED"
        if s in ("closed", "locked"):
            return "CLOSED"
        return "OPEN"  # "opened" -> OPEN

    @staticmethod
    def _norm_mergeable(mr: Row) -> str:
        status = (mr.get("detailed_merge_status") or mr.get("merge_status") or "").lower()
        if status in ("mergeable", "can_be_merged", "ci_must_pass", "ci_still_running"):
            return "MERGEABLE"
        if "conflict" in status or status == "cannot_be_merged":
            return "CONFLICTING"
        return "UNKNOWN"

    @staticmethod
    def _norm_pipeline(mr: Row) -> list[Row] | None:
        pipeline = mr.get("head_pipeline") or mr.get("pipeline")
        if not pipeline or not isinstance(pipeline, dict):
            return None
        status = (pipeline.get("status") or "").lower()
        _map: dict[str, str] = {
            "success": "SUCCESS",
            "failed": "FAILURE",
            "canceled": "CANCELLED",
            "skipped": "SKIPPED",
            "running": "IN_PROGRESS",
            "pending": "PENDING",
            "created": "PENDING",
            "manual": "NEUTRAL",
        }
        return [{"conclusion": _map.get(status, status.upper())}]

    @staticmethod
    def _split_notes(notes: list[Any]) -> tuple[list[Row], list[Row]]:
        """Split GitLab notes into (comments, reviews) with canonical keys."""
        comments: list[Row] = []
        reviews: list[Row] = []
        for n in notes:
            if not isinstance(n, dict) or n.get("system"):
                continue
            ts = n.get("created_at") or n.get("updated_at") or ""
            if n.get("type") == "DiffNote":
                reviews.append({"submittedAt": ts, "body": n.get("body", "")})
            else:
                comments.append({"createdAt": ts, "body": n.get("body", "")})
        return comments, reviews

    def _fetch_mr(self, slug: str, number: int, timeout: int = 30) -> tuple[int, Row | None, str]:
        """Fetch MR JSON + notes via GitLab REST API."""
        enc = self._encoded(slug)
        rc, out = self._api(f"projects/{enc}/merge_requests/{number}", timeout=timeout)
        if rc != 0:
            return rc, None, out
        try:
            mr: Row = json.loads(out)
        except ValueError:
            return 1, None, out

        # Notes are a separate endpoint.
        rc2, out2 = self._api(
            f"projects/{enc}/merge_requests/{number}/notes?sort=asc&per_page=100",
            timeout=timeout,
        )
        notes: list[Any] = []
        if rc2 == 0:
            try:
                parsed = json.loads(out2)
                if isinstance(parsed, list):
                    notes = parsed
            except ValueError:
                pass
        mr["_notes"] = notes
        return 0, mr, out

    # -- lifecycle ----------------------------------------------------------

    def create_pr(
        self,
        slug: str,
        branch: str,
        title: str,
        body_file: Path,
        cwd: str | None = None,
        timeout: int = 300,
    ) -> tuple[int, str]:
        body = ""
        with contextlib.suppress(OSError):
            body = Path(body_file).read_text()
        cmd = [
            "glab",
            "mr",
            "create",
            "--source-branch",
            branch,
            "--draft",
            "--title",
            title or branch,
            "--description",
            body,
            "-R",
            self._repo_flag(slug),
            "--yes",
        ]
        rc, out = run_cmd(cmd, cwd=cwd, timeout=timeout)
        if rc == 0:
            # glab prints the MR URL; fish it out.
            for line in reversed(out.strip().splitlines()):
                if "/-/merge_requests/" in line:
                    return 0, line.strip()
            return 0, out.strip().splitlines()[-1] if out.strip() else ""
        return rc, out

    def view_pr_sync(
        self, slug: str, number: int, timeout: int = 30
    ) -> tuple[int, Row | None, str]:
        rc, mr, raw = self._fetch_mr(slug, number, timeout=timeout)
        if rc != 0 or mr is None:
            return rc, None, raw
        comments, reviews = self._split_notes(mr.pop("_notes", []))
        return (
            0,
            {
                "state": self._norm_state(mr.get("state", "")),
                "mergedAt": mr.get("merged_at"),
                "mergeable": self._norm_mergeable(mr),
                "reviewDecision": None,  # no direct GitLab equivalent
                "statusCheckRollup": self._norm_pipeline(mr),
                "comments": comments,
                "reviews": reviews,
                "updatedAt": mr.get("updated_at", ""),
                "headRefName": mr.get("source_branch", ""),
            },
            raw,
        )

    def view_pr_engage(
        self, slug: str, number: int, timeout: int = 30
    ) -> tuple[int, Row | None, str]:
        rc, mr, raw = self._fetch_mr(slug, number, timeout=timeout)
        if rc != 0 or mr is None:
            return rc, None, raw
        comments, reviews = self._split_notes(mr.pop("_notes", []))
        return (
            0,
            {
                "title": mr.get("title", ""),
                "body": mr.get("description", ""),
                "comments": comments,
                "reviews": reviews,
                "statusCheckRollup": self._norm_pipeline(mr),
            },
            raw,
        )

    def comment_pr(
        self, slug: str, number: int, body_file: Path, timeout: int = 60
    ) -> tuple[int, str]:
        try:
            body = Path(body_file).read_text()
        except OSError:
            return 1, f"cannot read {body_file}"
        enc = self._encoded(slug)
        return self._api(
            f"projects/{enc}/merge_requests/{number}/notes",
            method="POST",
            fields={"body": body},
            timeout=timeout,
        )

    def close_pr(self, slug: str, number: int, comment: str, timeout: int = 60) -> tuple[int, str]:
        # Post comment, then close (glab mr close has no --comment flag).
        enc = self._encoded(slug)
        self._api(
            f"projects/{enc}/merge_requests/{number}/notes",
            method="POST",
            fields={"body": comment[:800]},
            timeout=timeout,
        )
        return run_cmd(
            [
                "glab",
                "mr",
                "close",
                str(number),
                "-R",
                self._repo_flag(slug),
            ],
            timeout=timeout,
        )


# ---------------------------------------------------------------------------
# Factory
# ---------------------------------------------------------------------------

_FORGES: dict[str, type[Forge]] = {
    "github": GitHubForge,
    "gitlab": GitLabForge,
}
FORGE_NAMES: tuple[str, ...] = tuple(_FORGES)


def _extract_host(url: str) -> str:
    """Extract hostname from a git remote URL."""
    m = re.match(r"^https?://([^/]+)", url)
    if m:
        return m.group(1)
    m = re.match(r"^git@([^:]+):", url)
    if m:
        return m.group(1)
    return "gitlab.com"


def detect_forge(url: str) -> str:
    """Best-effort forge type from a remote URL."""
    host = _extract_host(url).lower()
    if "github" in host:
        return "github"
    if "gitlab" in host:
        return "gitlab"
    return "github"


def forge_for(repo: Row) -> Forge:
    """Pick the right Forge implementation from a repo dict."""
    kind = repo.get("forge", "github")
    if kind == "gitlab":
        return GitLabForge(_extract_host(repo.get("url", "")))
    return GitHubForge()
