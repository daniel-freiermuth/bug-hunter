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

    def test_unparseable_has_no_host(self) -> None:
        # Not the literal "gitlab.com" it used to return: that read
        # every ssh:// URL as GitLab, including GitHub ones.
        assert _extract_host("bogus") == ""

    def test_ssh_scheme_and_port(self) -> None:
        assert _extract_host("ssh://git@github.com/acme/w.git") == "github.com"
        assert _extract_host("ssh://git@gitlab.example.com:2222/g/r.git") == "gitlab.example.com"
        assert _extract_host("http://github.com/a/b.git") == "github.com"


class TestDetectForge:
    def test_github_https(self) -> None:
        assert detect_forge("https://github.com/a/b") == "github"

    def test_github_ssh(self) -> None:
        assert detect_forge("git@github.com:a/b.git") == "github"

    def test_gitlab_https(self) -> None:
        assert detect_forge("https://gitlab.com/a/b") == "gitlab"

    def test_gitlab_self_hosted(self) -> None:
        assert detect_forge("https://gitlab.corp.net/a/b") == "gitlab"

    def test_unknown_host_defaults_gitlab(self) -> None:
        # GitHub cannot be self-hosted off a domain GitHub operates
        # without saying so; anything else can be a self-hosted GitLab.
        assert detect_forge("https://bitbucket.org/a/b") == "gitlab"
        assert detect_forge("https://git.internal/a/b") == "gitlab"

    def test_empty_defaults_gitlab(self) -> None:
        # No host takes the same remainder as any unrecognised host.
        assert detect_forge("") == "gitlab"

    def test_ssh_urls_detect_by_host(self) -> None:
        assert detect_forge("ssh://git@github.com/acme/widget.git") == "github"
        assert detect_forge("ssh://git@gitlab.example.com:2222/g/r.git") == "gitlab"

    def test_forge_comes_from_the_host_not_the_path(self) -> None:
        # A GitHub repo whose NAME contains "gitlab".
        assert detect_forge("https://github.com/acme/gitlab-ci-templates.git") == "github"
        assert detect_forge("https://GitLab.example.com/g/r.git") == "gitlab"


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


class TestGitHubEnterpriseHosts:
    """A host detect_forge calls GitHub must work with the GitHub ops.

    When only github.com parsed, an Enterprise repo was accepted at
    POST /api/repos, hunted and fixed, and then every PR action for it
    failed because its slug would not parse.
    """

    def test_owner_repo_carries_the_host(self) -> None:
        gh = GitHubForge()
        for url in (
            "https://github.corp.com/acme/widget.git",
            "ssh://git@github.corp.com:22/acme/widget.git",
            "git@github.corp.com:acme/widget.git",
        ):
            assert gh.owner_repo(url) == "github.corp.com/acme/widget", url
        assert gh.owner_repo("https://mycompany.ghe.com/a/w") == "mycompany.ghe.com/a/w"
        # github.com keeps the bare slug every stored PR already uses.
        assert gh.owner_repo("https://github.com/acme/widget.git") == "acme/widget"

    def test_non_github_hosts_stay_rejected(self) -> None:
        gh = GitHubForge()
        assert gh.owner_repo("https://gitlab.com/acme/widget") is None
        assert gh.owner_repo("https://code.corp.com/acme/widget") is None
        assert gh.parse_pr_url("https://gitlab.com/a/b/pull/7") is None
        # Handed back untouched rather than rewritten to github.com.
        assert gh.ssh_url("https://code.corp.com/a/b") == "https://code.corp.com/a/b"

    def test_pr_url_and_ssh_url_use_the_real_host(self) -> None:
        gh = GitHubForge()
        assert gh.parse_pr_url("https://github.corp.com/a/w/pull/7") == (
            "github.corp.com/a/w",
            7,
        )
        assert gh.parse_pr_url("https://github.com/a/w/pull/7") == ("a/w", 7)
        assert gh.ssh_url("https://github.corp.com/a/w") == "git@github.corp.com:a/w.git"
        assert gh.ssh_url("https://github.com/a/w") == "git@github.com:a/w.git"
