#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Forge tests — URL parsing, SSH transform, `detect_forge`.

use hunter::domain::ForgeName;
use hunter::forge::*;

// ============================================================
// GitHub — ssh_url
// ============================================================

#[test]
fn test_github_ssh_url_basic() {
    let gh = GitHubForge;
    assert_eq!(
        gh.ssh_url("https://github.com/owner/repo"),
        "git@github.com:owner/repo.git"
    );
}

#[test]
fn test_github_ssh_url_strips_dotgit() {
    let gh = GitHubForge;
    assert_eq!(
        gh.ssh_url("https://github.com/owner/repo.git"),
        "git@github.com:owner/repo.git"
    );
}

#[test]
fn test_github_ssh_url_strips_trailing_slash() {
    let gh = GitHubForge;
    assert_eq!(
        gh.ssh_url("https://github.com/owner/repo/"),
        "git@github.com:owner/repo.git"
    );
}

#[test]
fn test_github_ssh_url_non_github_passthrough() {
    let gh = GitHubForge;
    assert_eq!(
        gh.ssh_url("https://gitlab.com/owner/repo"),
        "https://gitlab.com/owner/repo"
    );
}

#[test]
fn test_github_ssh_url_already_ssh_passthrough() {
    let gh = GitHubForge;
    assert_eq!(
        gh.ssh_url("git@github.com:owner/repo.git"),
        "git@github.com:owner/repo.git"
    );
}

// ============================================================
// GitHub — owner_repo
// ============================================================

#[test]
fn test_github_owner_repo_https() {
    let gh = GitHubForge;
    assert_eq!(
        gh.owner_repo("https://github.com/acme/widgets"),
        Some(("acme".into(), "widgets".into()))
    );
}

#[test]
fn test_github_owner_repo_https_with_dotgit() {
    let gh = GitHubForge;
    assert_eq!(
        gh.owner_repo("https://github.com/acme/widgets.git"),
        Some(("acme".into(), "widgets".into()))
    );
}

#[test]
fn test_github_owner_repo_ssh() {
    let gh = GitHubForge;
    assert_eq!(
        gh.owner_repo("git@github.com:acme/widgets.git"),
        Some(("acme".into(), "widgets".into()))
    );
}

#[test]
fn test_github_owner_repo_invalid() {
    let gh = GitHubForge;
    assert_eq!(gh.owner_repo("https://gitlab.com/acme/widgets"), None);
    assert_eq!(gh.owner_repo("not-a-url"), None);
}

// ============================================================
// GitLab — ssh_url
// ============================================================

#[test]
fn test_gitlab_ssh_url_basic() {
    let gl = GitLabForge;
    assert_eq!(
        gl.ssh_url("https://gitlab.com/group/repo"),
        "git@gitlab.com:group/repo.git"
    );
}

#[test]
fn test_gitlab_ssh_url_nested_group() {
    let gl = GitLabForge;
    assert_eq!(
        gl.ssh_url("https://gitlab.com/group/sub/repo.git"),
        "git@gitlab.com:group/sub/repo.git"
    );
}

#[test]
fn test_gitlab_ssh_url_self_hosted() {
    let gl = GitLabForge;
    assert_eq!(
        gl.ssh_url("https://git.example.com/team/proj"),
        "git@git.example.com:team/proj.git"
    );
}

#[test]
fn test_gitlab_ssh_url_already_ssh_passthrough() {
    let gl = GitLabForge;
    // Already SSH — no https:// prefix, returned as-is
    assert_eq!(
        gl.ssh_url("git@gitlab.com:group/repo.git"),
        "git@gitlab.com:group/repo.git"
    );
}

// ============================================================
// GitLab — owner_repo
// ============================================================

#[test]
fn test_gitlab_owner_repo_https() {
    let gl = GitLabForge;
    assert_eq!(
        gl.owner_repo("https://gitlab.com/group/repo"),
        Some(("group".into(), "repo".into()))
    );
}

#[test]
fn test_gitlab_owner_repo_nested() {
    let gl = GitLabForge;
    assert_eq!(
        gl.owner_repo("https://gitlab.com/group/sub/repo"),
        Some(("group/sub".into(), "repo".into()))
    );
}

#[test]
fn test_gitlab_owner_repo_ssh() {
    let gl = GitLabForge;
    assert_eq!(
        gl.owner_repo("git@gitlab.com:group/repo.git"),
        Some(("group".into(), "repo".into()))
    );
}

#[test]
fn test_gitlab_owner_repo_invalid() {
    let gl = GitLabForge;
    assert_eq!(gl.owner_repo("not-a-url"), None);
}

// ============================================================
// detect_forge
// ============================================================

#[test]
fn test_detect_forge_github() {
    assert_eq!(
        detect_forge("https://github.com/owner/repo"),
        ForgeName::Github
    );
    assert_eq!(
        detect_forge("git@github.com:owner/repo.git"),
        ForgeName::Github
    );
}

#[test]
fn test_detect_forge_gitlab() {
    assert_eq!(
        detect_forge("https://gitlab.com/group/repo"),
        ForgeName::Gitlab
    );
    assert_eq!(
        detect_forge("git@gitlab.com:group/repo.git"),
        ForgeName::Gitlab
    );
}

// ============================================================
// forge_for factory
// ============================================================

#[test]
fn test_forge_for_github() {
    let f = forge_for(ForgeName::Github);
    assert_eq!(
        f.ssh_url("https://github.com/a/b"),
        "git@github.com:a/b.git"
    );
}

#[test]
fn test_forge_for_gitlab() {
    let f = forge_for(ForgeName::Gitlab);
    assert_eq!(
        f.ssh_url("https://gitlab.com/a/b"),
        "git@gitlab.com:a/b.git"
    );
}

#[test]
fn test_forge_for_all_variants_covered() {
    // Every ForgeName variant should produce a working Forge.
    for name in ForgeName::ALL {
        let _f = forge_for(name);
    }
}

#[test]
fn test_forge_name_variants() {
    assert_eq!(ForgeName::ALL.len(), 2);
    assert!(ForgeName::ALL.contains(&ForgeName::Github));
    assert!(ForgeName::ALL.contains(&ForgeName::Gitlab));
}

// ---------------------------------------------------------------------------
// Forge detection and URL parsing across every accepted URL form
// ---------------------------------------------------------------------------

/// The forge comes from the host, not from a substring of the whole URL.
///
/// A substring test called `https://github.com/acme/gitlab-ci-templates`
/// a GitLab repo, and the consequence is not cosmetic: `create_pr` then
/// runs `glab` against a GitHub remote and `gitlab_api` passes
/// `--hostname github.com`, so every PR operation for that repo fails.
#[test]
fn forge_is_detected_from_the_host() {
    for (url, want) in [
        ("https://github.com/acme/widget.git", ForgeName::Github),
        // The repo NAME contains "gitlab"; the host does not.
        (
            "https://github.com/acme/gitlab-ci-templates.git",
            ForgeName::Github,
        ),
        ("git@github.com:acme/widget.git", ForgeName::Github),
        ("ssh://git@github.com/acme/widget.git", ForgeName::Github),
        ("http://github.com/acme/widget.git", ForgeName::Github),
        ("https://gitlab.com/g/sub/r.git", ForgeName::Gitlab),
        ("https://gitlab.example.com/g/r.git", ForgeName::Gitlab),
        // Hosts are matched case-insensitively.
        ("https://GitLab.example.com/g/r.git", ForgeName::Gitlab),
        (
            "ssh://git@gitlab.example.com:2222/g/r.git",
            ForgeName::Gitlab,
        ),
        // Self-hosted GitLab under a company domain.
        ("https://gitlab.mycompany.com/g/r.git", ForgeName::Gitlab),
        (
            "ssh://git@gitlab.mycompany.com:2222/g/r.git",
            ForgeName::Gitlab,
        ),
        // GitHub Enterprise Cloud: <org>.ghe.com, a domain GitHub runs.
        (
            "https://mycompany.ghe.com/acme/widget.git",
            ForgeName::Github,
        ),
        ("git@mycompany.ghe.com:acme/widget.git", ForgeName::Github),
        // GitHub Enterprise Server, named by convention.
        (
            "https://github.mycompany.com/acme/widget.git",
            ForgeName::Github,
        ),
        // A host that names neither forge is far likelier to be a
        // self-hosted GitLab than a GitHub, which cannot be self-hosted
        // off a GitHub-operated domain without saying so.
        ("https://code.mycompany.com/g/r.git", ForgeName::Gitlab),
        ("https://git.internal/acme/widget.git", ForgeName::Gitlab),
        ("https://bitbucket.org/a/b.git", ForgeName::Gitlab),
        // Labels, not substrings: a host merely CONTAINING the letters
        // is not GitHub. (A sanity bound, not an anti-spoofing measure
        // -- the URL comes from the operator adding their own repo, and
        // `github.com.evil.example` would still read as GitHub. Nothing
        // here defends against a hostile URL.)
        ("https://notgithub.com/acme/widget.git", ForgeName::Gitlab),
        ("https://notghe.com/acme/widget.git", ForgeName::Gitlab),
        // An unparseable URL has no host, so it takes the remainder too.
        ("bogus", ForgeName::Gitlab),
    ] {
        assert_eq!(detect_forge(url), want, "detect_forge({url})");
    }
}

/// Every URL form the add endpoint accepts must also be parseable here.
///
/// `valid_repo_url` accepts http/https/ssh and the scp-like form, and
/// `post_test` asserts `ssh://git@github.com/...` is created with a 201.
/// If the parsers disagree, `create_pr` still works (it uses the worktree
/// remote) but `sync_prs`, engage and withdraw all fail for that repo
/// forever — a PR that is opened and then never tracked.
#[test]
fn every_accepted_url_form_parses_to_owner_and_repo() {
    let gh = GitHubForge;
    for url in [
        "https://github.com/acme/widget.git",
        "http://github.com/acme/widget.git",
        "git@github.com:acme/widget.git",
        "ssh://git@github.com/acme/widget.git",
        "ssh://github.com/acme/widget.git",
        "ssh://git@github.com:22/acme/widget.git",
    ] {
        assert_eq!(
            gh.owner_repo(url),
            Some(("acme".to_owned(), "widget".to_owned())),
            "owner_repo({url})"
        );
    }

    // GitLab splits the project path into (namespace, project), so a
    // nested group stays in the namespace half.
    let gl = GitLabForge;
    for url in [
        "https://gitlab.example.com/g/sub/r.git",
        "http://gitlab.example.com/g/sub/r.git",
        "git@gitlab.example.com:g/sub/r.git",
        "ssh://git@gitlab.example.com/g/sub/r.git",
        "ssh://git@gitlab.example.com:2222/g/sub/r.git",
    ] {
        assert_eq!(
            gl.owner_repo(url),
            Some(("g/sub".to_owned(), "r".to_owned())),
            "owner_repo({url})"
        );
    }
}

/// A host that `detect_forge` calls GitHub must be usable by the GitHub
/// operations. When only `github.com` parsed, every PR action for an
/// Enterprise repo failed with `cannot parse owner/repo` — the repo was
/// accepted at POST /api/repos, hunted, fixed, and then could never have
/// its PR opened or tracked.
#[test]
fn github_enterprise_hosts_survive_the_whole_chain() {
    let gh = GitHubForge;
    for (url, want_owner) in [
        (
            "https://github.corp.com/acme/widget.git",
            "github.corp.com/acme",
        ),
        (
            "https://mycompany.ghe.com/acme/widget.git",
            "mycompany.ghe.com/acme",
        ),
        (
            "ssh://git@github.corp.com:22/acme/widget.git",
            "github.corp.com/acme",
        ),
        (
            "git@mycompany.ghe.com:acme/widget.git",
            "mycompany.ghe.com/acme",
        ),
    ] {
        assert_eq!(detect_forge(url), ForgeName::Github, "detect_forge({url})");
        assert_eq!(
            gh.owner_repo(url),
            Some((want_owner.to_owned(), "widget".to_owned())),
            "owner_repo({url})"
        );
    }

    // The slug goes straight into `gh -R`, whose accepted form is
    // [HOST/]OWNER/REPO, so the host has to ride along.
    assert_eq!(
        gh.parse_pr_url("https://github.corp.com/acme/widget/pull/7"),
        Some(("github.corp.com/acme/widget".to_owned(), 7))
    );
    // github.com keeps the bare two-part slug.
    assert_eq!(
        gh.parse_pr_url("https://github.com/acme/widget/pull/7"),
        Some(("acme/widget".to_owned(), 7))
    );

    // Pushing must target the internal host, not github.com.
    assert_eq!(
        gh.ssh_url("https://github.corp.com/acme/widget"),
        "git@github.corp.com:acme/widget.git"
    );
    assert_eq!(
        gh.ssh_url("https://github.com/acme/widget"),
        "git@github.com:acme/widget.git"
    );

    // Still not a blanket accept: a non-GitHub host stays rejected, or
    // GitHubForge would claim every self-hosted GitLab.
    assert_eq!(gh.owner_repo("https://gitlab.com/acme/widget"), None);
    assert_eq!(gh.owner_repo("https://code.corp.com/acme/widget"), None);
    assert_eq!(
        gh.parse_pr_url("https://gitlab.com/acme/widget/pull/7"),
        None
    );
    assert_eq!(
        gh.ssh_url("https://code.corp.com/acme/widget"),
        "https://code.corp.com/acme/widget",
        "an unrecognised host must be handed back untouched, not rewritten"
    );
}
