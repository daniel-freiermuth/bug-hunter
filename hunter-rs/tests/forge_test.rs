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

#[test]
fn test_detect_forge_default() {
    // Unknown hosts → default "github"
    assert_eq!(detect_forge("https://example.com/repo"), ForgeName::Github);
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
