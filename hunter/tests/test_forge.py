"""Tests for hunter.forge — URL parsing, normalisation, and factory."""

from __future__ import annotations

import pytest

from hunter.forge import (
    GitHubForge,
    GitLabForge,
    _extract_host,
    detect_forge,
    forge_for,
)

# ---------------------------------------------------------------------------
# GitHubForge
# ---------------------------------------------------------------------------


class TestGitHubForgeSSHUrl:
    def setup_method(self) -> None:
        self.f = GitHubForge()

    def test_https_to_ssh(self) -> None:
        assert self.f.ssh_url("https://github.com/owner/repo") == "git@github.com:owner/repo.git"

    def test_https_with_git_suffix(self) -> None:
        assert (
            self.f.ssh_url("https://github.com/owner/repo.git") == "git@github.com:owner/repo.git"
        )

    def test_https_trailing_slash(self) -> None:
        assert self.f.ssh_url("https://github.com/owner/repo/") == "git@github.com:owner/repo.git"

    def test_non_github_passthrough(self) -> None:
        url = "https://gitlab.com/owner/repo"
        assert self.f.ssh_url(url) == url

    def test_ssh_passthrough(self) -> None:
        url = "git@github.com:owner/repo.git"
        assert self.f.ssh_url(url) == url


class TestGitHubForgeOwnerRepo:
    def setup_method(self) -> None:
        self.f = GitHubForge()

    def test_https(self) -> None:
        assert self.f.owner_repo("https://github.com/acme/widgets") == "acme/widgets"

    def test_https_git_suffix(self) -> None:
        assert self.f.owner_repo("https://github.com/acme/widgets.git") == "acme/widgets"

    def test_ssh(self) -> None:
        assert self.f.owner_repo("git@github.com:acme/widgets.git") == "acme/widgets"

    def test_ssh_no_suffix(self) -> None:
        assert self.f.owner_repo("git@github.com:acme/widgets") == "acme/widgets"

    def test_non_github_returns_none(self) -> None:
        assert self.f.owner_repo("https://gitlab.com/acme/widgets") is None


class TestGitHubForgeParsePR:
    def setup_method(self) -> None:
        self.f = GitHubForge()

    def test_basic(self) -> None:
        assert self.f.parse_pr_url("https://github.com/acme/widgets/pull/42") == (
            "acme/widgets",
            42,
        )

    def test_with_extra_path(self) -> None:
        assert self.f.parse_pr_url("https://github.com/acme/widgets/pull/7/files") == (
            "acme/widgets",
            7,
        )

    def test_non_pr_url(self) -> None:
        assert self.f.parse_pr_url("https://github.com/acme/widgets/issues/1") is None

    def test_non_github(self) -> None:
        assert self.f.parse_pr_url("https://gitlab.com/acme/widgets/-/merge_requests/1") is None


# ---------------------------------------------------------------------------
# GitLabForge
# ---------------------------------------------------------------------------


class TestGitLabForgeSSHUrl:
    def setup_method(self) -> None:
        self.f = GitLabForge()

    def test_https_to_ssh(self) -> None:
        assert (
            self.f.ssh_url("https://gitlab.com/group/project") == "git@gitlab.com:group/project.git"
        )

    def test_https_git_suffix(self) -> None:
        assert (
            self.f.ssh_url("https://gitlab.com/group/project.git")
            == "git@gitlab.com:group/project.git"
        )

    def test_trailing_slash(self) -> None:
        assert (
            self.f.ssh_url("https://gitlab.com/group/project/")
            == "git@gitlab.com:group/project.git"
        )

    def test_subgroup(self) -> None:
        assert (
            self.f.ssh_url("https://gitlab.com/group/sub/project")
            == "git@gitlab.com:group/sub/project.git"
        )

    def test_non_gitlab_passthrough(self) -> None:
        url = "https://github.com/owner/repo"
        assert self.f.ssh_url(url) == url

    def test_self_hosted(self) -> None:
        f = GitLabForge("git.corp.com")
        assert f.ssh_url("https://git.corp.com/team/proj") == "git@git.corp.com:team/proj.git"


class TestGitLabForgeOwnerRepo:
    def setup_method(self) -> None:
        self.f = GitLabForge()

    def test_https(self) -> None:
        assert self.f.owner_repo("https://gitlab.com/group/project") == "group/project"

    def test_https_git_suffix(self) -> None:
        assert self.f.owner_repo("https://gitlab.com/group/project.git") == "group/project"

    def test_ssh(self) -> None:
        assert self.f.owner_repo("git@gitlab.com:group/project.git") == "group/project"

    def test_subgroup(self) -> None:
        assert self.f.owner_repo("https://gitlab.com/a/b/c") == "a/b/c"

    def test_non_gitlab_returns_none(self) -> None:
        assert self.f.owner_repo("https://github.com/a/b") is None

    def test_self_hosted(self) -> None:
        f = GitLabForge("git.corp.com")
        assert f.owner_repo("https://git.corp.com/team/proj") == "team/proj"
        assert f.owner_repo("git@git.corp.com:team/proj.git") == "team/proj"


class TestGitLabForgeParsePR:
    def setup_method(self) -> None:
        self.f = GitLabForge()

    def test_basic(self) -> None:
        assert self.f.parse_pr_url("https://gitlab.com/group/project/-/merge_requests/99") == (
            "group/project",
            99,
        )

    def test_subgroup(self) -> None:
        assert self.f.parse_pr_url("https://gitlab.com/a/b/c/-/merge_requests/5") == ("a/b/c", 5)

    def test_non_mr_url(self) -> None:
        assert self.f.parse_pr_url("https://gitlab.com/a/b/-/issues/1") is None

    def test_self_hosted(self) -> None:
        f = GitLabForge("git.corp.com")
        assert f.parse_pr_url("https://git.corp.com/team/proj/-/merge_requests/42") == (
            "team/proj",
            42,
        )


class TestGitLabNormState:
    @pytest.mark.parametrize(
        ("raw", "expected"),
        [
            ("opened", "OPEN"),
            ("Opened", "OPEN"),
            ("merged", "MERGED"),
            ("Merged", "MERGED"),
            ("closed", "CLOSED"),
            ("locked", "CLOSED"),
            ("", "OPEN"),
            ("anything_else", "OPEN"),
        ],
    )
    def test_mapping(self, raw: str, expected: str) -> None:
        assert GitLabForge._norm_state(raw) == expected


class TestGitLabNormMergeable:
    @pytest.mark.parametrize(
        ("status_key", "status_val", "expected"),
        [
            ("detailed_merge_status", "mergeable", "MERGEABLE"),
            ("merge_status", "can_be_merged", "MERGEABLE"),
            ("detailed_merge_status", "ci_must_pass", "MERGEABLE"),
            ("detailed_merge_status", "ci_still_running", "MERGEABLE"),
            ("detailed_merge_status", "has_conflict", "CONFLICTING"),
            ("merge_status", "cannot_be_merged", "CONFLICTING"),
            ("detailed_merge_status", "checking", "UNKNOWN"),
            ("detailed_merge_status", "", "UNKNOWN"),
        ],
    )
    def test_mapping(self, status_key: str, status_val: str, expected: str) -> None:
        mr = {status_key: status_val}
        assert GitLabForge._norm_mergeable(mr) == expected

    def test_empty_mr(self) -> None:
        assert GitLabForge._norm_mergeable({}) == "UNKNOWN"

    def test_detailed_takes_precedence(self) -> None:
        mr = {
            "detailed_merge_status": "mergeable",
            "merge_status": "cannot_be_merged",
        }
        assert GitLabForge._norm_mergeable(mr) == "MERGEABLE"


# ---------------------------------------------------------------------------
# _extract_host / detect_forge / forge_for
# ---------------------------------------------------------------------------


class TestExtractHost:
    def test_https(self) -> None:
        assert _extract_host("https://github.com/a/b") == "github.com"

    def test_http(self) -> None:
        assert _extract_host("http://gitlab.corp.net/a/b") == "gitlab.corp.net"

    def test_ssh(self) -> None:
        assert _extract_host("git@github.com:a/b.git") == "github.com"

    def test_self_hosted_ssh(self) -> None:
        assert _extract_host("git@git.corp.com:team/proj.git") == "git.corp.com"

    def test_fallback(self) -> None:
        assert _extract_host("bogus") == "gitlab.com"


class TestDetectForge:
    def test_github_https(self) -> None:
        assert detect_forge("https://github.com/a/b") == "github"

    def test_github_ssh(self) -> None:
        assert detect_forge("git@github.com:a/b.git") == "github"

    def test_gitlab_https(self) -> None:
        assert detect_forge("https://gitlab.com/a/b") == "gitlab"

    def test_gitlab_self_hosted(self) -> None:
        assert detect_forge("https://gitlab.corp.net/a/b") == "gitlab"

    def test_unknown_defaults_github(self) -> None:
        assert detect_forge("https://bitbucket.org/a/b") == "github"

    def test_empty_defaults_gitlab(self) -> None:
        # empty → _extract_host fallback "gitlab.com" → "gitlab"
        assert detect_forge("") == "gitlab"


class TestForgeFor:
    def test_github_default(self) -> None:
        f = forge_for({"url": "https://github.com/a/b"})
        assert isinstance(f, GitHubForge)

    def test_github_explicit(self) -> None:
        f = forge_for({"forge": "github", "url": "https://github.com/a/b"})
        assert isinstance(f, GitHubForge)

    def test_gitlab(self) -> None:
        f = forge_for({"forge": "gitlab", "url": "https://gitlab.com/g/p"})
        assert isinstance(f, GitLabForge)
        assert f.host == "gitlab.com"

    def test_gitlab_self_hosted(self) -> None:
        f = forge_for({"forge": "gitlab", "url": "https://git.corp.com/t/p"})
        assert isinstance(f, GitLabForge)
        assert f.host == "git.corp.com"

    def test_missing_forge_key(self) -> None:
        f = forge_for({"url": "https://example.com/a/b"})
        assert isinstance(f, GitHubForge)

    def test_empty_dict(self) -> None:
        f = forge_for({})
        assert isinstance(f, GitHubForge)
