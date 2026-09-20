//! Forge abstraction — GitHub / GitLab PR/MR lifecycle via CLI tools.
//! Port of hunter/forge.py. Methods shell out to `gh` / `glab`.

use std::path::Path;

use serde::Deserialize;

use crate::domain::ForgeName;
use crate::util::{run_cmd, run_cmd_in};

/// Git forge PR/MR lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrState {
    #[default]
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
        }
    }
}

/// Whether the PR can be merged cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mergeable {
    #[default]
    Unknown,
    Mergeable,
    Conflicting,
}

impl Mergeable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mergeable => "MERGEABLE",
            Self::Conflicting => "CONFLICTING",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// Review decision on the PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewDecision {
    #[default]
    None,
    Approved,
    ChangesRequested,
}

/// Check/CI conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConclusion {
    Success,
    Failure,
    TimedOut,
    Cancelled,
    Neutral,
    Skipped,
    Pending,
    Other,
}

impl CheckConclusion {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "SUCCESS" => Self::Success,
            "FAILURE" => Self::Failure,
            "TIMED_OUT" => Self::TimedOut,
            "CANCELLED" => Self::Cancelled,
            "NEUTRAL" => Self::Neutral,
            "SKIPPED" => Self::Skipped,
            "PENDING" | "" => Self::Pending,
            _ => Self::Other,
        }
    }
    pub fn is_failing(self) -> bool {
        matches!(self, Self::Failure | Self::TimedOut | Self::Cancelled)
    }
    pub fn is_passing(self) -> bool {
        matches!(self, Self::Success | Self::Neutral | Self::Skipped)
    }
}

// ---------------------------------------------------------------------------
// Typed API response structs
// ---------------------------------------------------------------------------

/// Author in GitHub API responses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GhAuthor {
    #[serde(default)]
    pub login: String,
}

/// Comment from GitHub's `pr.comments` array.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhComment {
    #[serde(default)]
    pub author: Option<GhAuthor>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
}

/// Review from GitHub's `pr.reviews` array.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhReview {
    #[serde(default)]
    pub author: Option<GhAuthor>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub submitted_at: String,
    #[serde(default)]
    pub state: String,
}

/// Status check / CI check entry.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GhCheckRun {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

// -- GitLab API types (normalized into Gh* types at parse boundary) ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlAuthor {
    #[serde(default)]
    pub username: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlNote {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub author: Option<GlAuthor>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub system: bool,
    #[serde(default, rename = "type")]
    pub note_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlPipeline {
    #[serde(default)]
    status: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GlMergeRequest {
    #[serde(default)]
    state: String,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    #[serde(default)]
    merge_status: Option<String>,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    head_pipeline: Option<GlPipeline>,
    #[serde(default)]
    pipeline: Option<GlPipeline>,
}

/// PR view data returned by `view_pr_sync` and `view_pr_engage`.
#[derive(Debug, Clone, Default)]
pub struct PrView {
    pub state: PrState,
    pub mergeable: Mergeable,
    pub review_decision: ReviewDecision,
    pub head_ref: String,
    pub head_sha: String,
    pub updated_at: String,
    pub title: String,
    pub body: String,
    pub status_check_rollup: Vec<GhCheckRun>,
    pub comments: Vec<GhComment>,
    pub reviews: Vec<GhReview>,
}

pub trait Forge: Send + Sync {
    /// Git SSH URL for push operations.
    fn ssh_url(&self, https_url: &str) -> String;
    /// owner/repo from a remote URL.
    fn owner_repo(&self, url: &str) -> Option<(String, String)>;
    /// Extract (`owner_repo_slug`, `pr_number`) from a PR/MR web URL.
    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)>;
    /// Create a draft PR/MR. Returns the PR URL.
    fn create_pr(
        &self,
        repo_path: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<String>;
    /// Lightweight PR view for `sync_prs` (state, checks, mergeable).
    fn view_pr_sync(&self, url: &str, pr_number: i64) -> anyhow::Result<PrView>;
    /// Heavier PR view for engage (includes comments/reviews).
    fn view_pr_engage(&self, url: &str, pr_number: i64) -> anyhow::Result<PrView>;
    /// Post a comment on a PR/MR.
    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()>;
    /// Close a PR/MR with a comment explaining why.
    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()>;
    /// Push to the remote (--force to raw SSH URL).
    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// [`run_cmd`] in a working directory. Every `git`/`gh`/`glab` call here
/// runs inside a worktree, so the cwd is not optional for this module.
fn run_cmd_cwd(argv: &[&str], cwd: &Path, timeout_s: u64) -> (i32, String) {
    run_cmd_in(argv, Some(cwd), timeout_s)
}

/// Whether `host` is a GitHub instance: `github.com`, an Enterprise
/// Server install (which by convention carries a `github` label, as in
/// `github.corp.com`), or Enterprise Cloud's `<org>.ghe.com`.
///
/// Labels, not substrings, so `notgithub.com` does not qualify. This is
/// the single definition of "is GitHub" — `detect_forge` classifying a
/// host that the GitHub operations then reject is how every PR action
/// for an Enterprise repo used to fail with `cannot parse owner/repo`.
fn is_github_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    labels.contains(&"github") || labels.ends_with(&["ghe", "com"])
}

/// Parse owner/repo from a GitHub-style URL (HTTPS or SSH).
///
/// For an Enterprise host the owner half carries the host, giving
/// `gh`'s `[HOST/]OWNER/REPO` form — every consumer formats the pair
/// straight into `-R`, and without the host `gh` would talk to
/// github.com about a repo that only exists on the internal instance.
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    // Every form `valid_repo_url` accepts, not just the two most common:
    // an `ssh://` or `http://` remote that the add endpoint takes with a
    // 201 must not then be unparseable here, or the repo gets a PR that
    // `sync_prs` can never track.
    let (host, path) = url_host_path(url)?;
    if !is_github_host(host) {
        return None;
    }
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let owner = if host.eq_ignore_ascii_case("github.com") {
        owner.to_owned()
    } else {
        format!("{}/{owner}", host.to_ascii_lowercase())
    };
    Some((owner, repo.to_owned()))
}

/// Split a remote URL into (host, path), for every form the write path
/// accepts: `https://`, `http://`, `ssh://` and scp-like `git@host:path`.
/// Any `user@` and `:port` are stripped from the host.
fn url_host_path(url: &str) -> Option<(&str, &str)> {
    let host = url_host(url)?;
    let after_scheme = ["https://", "http://", "ssh://"]
        .iter()
        .find_map(|p| url.strip_prefix(p));
    let path = match after_scheme {
        // Past the authority, which is everything up to the first slash.
        Some(rest) => rest.split_once('/').map(|(_, p)| p)?,
        None => url.split_once(':').map(|(_, p)| p)?,
    };
    (!path.is_empty()).then_some((host, path))
}

/// Extract (host, `project_path`) from a GitLab-style URL.
fn gitlab_host_path(url: &str) -> Option<(&str, &str)> {
    let (host, path) = url_host_path(url)?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    (!path.is_empty()).then_some((host, path))
}

/// Build a `gh pr view --json` call and parse the `PrView` from JSON.
fn gh_pr_view(slug: &str, pr_number: i64, fields: &str) -> anyhow::Result<PrView> {
    let num = pr_number.to_string();
    let (rc, out) = run_cmd(
        &["gh", "pr", "view", &num, "-R", slug, "--json", fields],
        30,
    );
    if rc != 0 {
        anyhow::bail!("gh pr view failed (rc={rc}): {out}");
    }
    let v: serde_json::Value = serde_json::from_str(out.trim())?;
    let str_field = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    Ok(PrView {
        state: match str_field("state").to_ascii_uppercase().as_str() {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::Open,
        },
        mergeable: match str_field("mergeable").to_ascii_uppercase().as_str() {
            "MERGEABLE" => Mergeable::Mergeable,
            "CONFLICTING" => Mergeable::Conflicting,
            _ => Mergeable::Unknown,
        },
        review_decision: match str_field("reviewDecision").to_ascii_uppercase().as_str() {
            "APPROVED" => ReviewDecision::Approved,
            "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
            _ => ReviewDecision::None,
        },
        head_ref: str_field("headRefName").to_owned(),
        head_sha: str_field("headRefOid").to_owned(),
        updated_at: str_field("updatedAt").to_owned(),
        title: str_field("title").to_owned(),
        body: str_field("body").to_owned(),
        status_check_rollup: serde_json::from_value(
            v.get("statusCheckRollup").cloned().unwrap_or_default(),
        )
        .unwrap_or_default(),
        comments: serde_json::from_value(v.get("comments").cloned().unwrap_or_default())
            .unwrap_or_default(),
        reviews: serde_json::from_value(v.get("reviews").cloned().unwrap_or_default())
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// GitLab normalisation helpers (`forge.GitLabForge._norm_*`, `_split_notes`)
// ---------------------------------------------------------------------------

fn gitlab_norm_state(raw: &str) -> PrState {
    match raw.to_ascii_lowercase().as_str() {
        "merged" => PrState::Merged,
        "closed" | "locked" => PrState::Closed,
        _ => PrState::Open,
    }
}

fn gitlab_norm_mergeable(mr: &GlMergeRequest) -> Mergeable {
    let status = mr
        .detailed_merge_status
        .as_deref()
        .or(mr.merge_status.as_deref())
        .unwrap_or("")
        .to_ascii_lowercase();
    match status.as_str() {
        "mergeable" | "can_be_merged" | "ci_must_pass" | "ci_still_running" => Mergeable::Mergeable,
        s if s.contains("conflict") || s == "cannot_be_merged" => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    }
}

fn gitlab_norm_pipeline(mr: &GlMergeRequest) -> Vec<GhCheckRun> {
    let pipeline = mr.head_pipeline.as_ref().or(mr.pipeline.as_ref());
    let Some(pipeline) = pipeline else {
        return Vec::new();
    };
    let status = pipeline.status.to_ascii_lowercase();
    let conclusion = match status.as_str() {
        "success" => "SUCCESS",
        "failed" => "FAILURE",
        "canceled" => "CANCELLED",
        "skipped" => "SKIPPED",
        "running" => "IN_PROGRESS",
        "pending" | "created" => "PENDING",
        "manual" => "NEUTRAL",
        _ => {
            return vec![GhCheckRun {
                conclusion: Some(status.to_ascii_uppercase()),
                ..Default::default()
            }];
        }
    };
    vec![GhCheckRun {
        conclusion: Some(conclusion.to_owned()),
        ..Default::default()
    }]
}

/// Split GitLab notes into (comments, reviews) normalized to Gh types.
fn gitlab_split_notes(notes: &[GlNote]) -> (Vec<GhComment>, Vec<GhReview>) {
    let mut comments = Vec::new();
    let mut reviews = Vec::new();
    for n in notes {
        if n.system {
            continue;
        }
        let ts = if n.created_at.is_empty() {
            &n.updated_at
        } else {
            &n.created_at
        };
        let author = n.author.as_ref().map(|a| GhAuthor {
            login: a.username.clone(),
        });
        if n.note_type.as_deref() == Some("DiffNote") {
            reviews.push(GhReview {
                submitted_at: ts.clone(),
                body: n.body.clone(),
                author,
                state: String::new(),
            });
        } else {
            comments.push(GhComment {
                created_at: ts.clone(),
                body: n.body.clone(),
                author,
            });
        }
    }
    (comments, reviews)
}

/// Fetch GitLab MR + notes via `glab api`, returning typed structs.
fn gitlab_fetch_mr(
    url: &str,
    slug: &str,
    number: i64,
) -> anyhow::Result<(GlMergeRequest, Vec<GlNote>)> {
    let enc = slug.replace('/', "%2F");
    let api_path = format!("projects/{enc}/merge_requests/{number}");
    let (rc, out) = gitlab_api(url, &api_path, None, &[]);
    if rc != 0 {
        anyhow::bail!("glab api {api_path} failed (rc={rc}): {out}");
    }
    let mr: GlMergeRequest = serde_json::from_str(out.trim())?;

    // Notes are a separate endpoint.
    let notes_path = format!("projects/{enc}/merge_requests/{number}/notes?sort=asc&per_page=100");
    let (rc2, out2) = gitlab_api(url, &notes_path, None, &[]);
    let notes: Vec<GlNote> = if rc2 == 0 {
        serde_json::from_str(out2.trim()).unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok((mr, notes))
}

/// Run `glab api <path>`, adding --hostname for self-hosted instances.
fn gitlab_api(
    url: &str,
    api_path: &str,
    method: Option<&str>,
    fields: &[(&str, &str)],
) -> (i32, String) {
    let mut args: Vec<String> = vec!["glab".into(), "api".into(), api_path.into()];
    if let Some(m) = method {
        args.extend(["--method".into(), m.into()]);
    }
    for (k, v) in fields {
        args.extend(["-f".into(), format!("{k}={v}")]);
    }
    if let Some((host, _)) = gitlab_host_path(url)
        && host != "gitlab.com"
    {
        args.extend(["--hostname".into(), host.into()]);
    }
    let refs: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
    run_cmd(&refs, 30)
}

// ---------------------------------------------------------------------------
// GitHub (via `gh` CLI) — `forge.GitHubForge`
// ---------------------------------------------------------------------------

pub struct GitHubForge;
pub struct GitLabForge;

impl Forge for GitHubForge {
    fn ssh_url(&self, https_url: &str) -> String {
        // Enterprise hosts too: a self-hosted GitHub's clone URL is
        // `git@<its host>:owner/repo.git`, and rewriting it to
        // github.com would push an internal repo at the public forge.
        if https_url.starts_with("https://")
            && let Some((host, path)) = url_host_path(https_url)
            && is_github_host(host)
        {
            let path = path.trim_end_matches('/').trim_end_matches(".git");
            if !path.is_empty() {
                return format!("git@{host}:{path}.git");
            }
        }
        https_url.to_owned()
    }

    fn owner_repo(&self, url: &str) -> Option<(String, String)> {
        github_owner_repo(url)
    }

    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)> {
        // https://<github host>/owner/repo/pull/123
        let (host, path) = url_host_path(url)?;
        if !url.starts_with("https://") || !is_github_host(host) {
            return None;
        }
        let (slug, num_part) = path.rsplit_once("/pull/")?;
        let num: i64 = num_part.split(&['/', '?', '#'][..]).next()?.parse().ok()?;
        // Same `[HOST/]OWNER/REPO` slug the owner_repo pair produces,
        // since both end up in `gh -R`.
        let slug = if host.eq_ignore_ascii_case("github.com") {
            slug.to_owned()
        } else {
            format!("{}/{slug}", host.to_ascii_lowercase())
        };
        Some((slug, num))
    }

    fn create_pr(
        &self,
        repo_path: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        let (rc, out) = run_cmd_cwd(
            &[
                "gh",
                "pr",
                "create",
                "--draft",
                "--head",
                head,
                "--base",
                base,
                "--title",
                if title.is_empty() { head } else { title },
                "--body",
                body,
            ],
            repo_path,
            300,
        );
        if rc != 0 {
            anyhow::bail!("gh pr create failed (rc={rc}): {out}");
        }
        // PR URL is the last non-empty line of output.
        let url = out.trim().lines().last().unwrap_or("").to_owned();
        Ok(url)
    }

    fn view_pr_sync(&self, url: &str, pr_number: i64) -> anyhow::Result<PrView> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        gh_pr_view(
            &slug,
            pr_number,
            "state,mergedAt,mergeable,reviewDecision,statusCheckRollup,comments,reviews,updatedAt,headRefName,headRefOid",
        )
    }

    fn view_pr_engage(&self, url: &str, pr_number: i64) -> anyhow::Result<PrView> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        gh_pr_view(
            &slug,
            pr_number,
            "title,body,comments,reviews,statusCheckRollup,headRefName,headRefOid",
        )
    }

    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()> {
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(
            &["gh", "pr", "comment", &num, "-R", &slug, "--body", body],
            60,
        );
        if rc != 0 {
            anyhow::bail!("gh pr comment failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()> {
        // Post the withdrawal reason as a comment first, then close. Only
        // close once it lands — a lost reason must not become a silent close.
        // Truncate to 800 chars to avoid CLI arg-length limits.
        let truncated: String = comment.chars().take(800).collect();
        if !truncated.is_empty() {
            self.comment_pr(url, pr_number, &truncated)?;
        }
        let (owner, repo) = self
            .owner_repo(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse owner/repo from {url}"))?;
        let slug = format!("{owner}/{repo}");
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(&["gh", "pr", "close", &num, "-R", &slug], 60);
        if rc != 0 {
            anyhow::bail!("gh pr close failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()> {
        let ssh = self.ssh_url(url);
        let refspec = format!("HEAD:{branch}");
        let (rc, out) = run_cmd_cwd(&["git", "push", "--force", &ssh, &refspec], repo_path, 120);
        if rc != 0 {
            anyhow::bail!("git push failed (rc={rc}): {out}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitLab (via `glab` CLI + REST API) — `forge.GitLabForge`
// ---------------------------------------------------------------------------

impl Forge for GitLabForge {
    fn ssh_url(&self, https_url: &str) -> String {
        if let Some((host, path)) = gitlab_host_path(https_url)
            && https_url.starts_with("https://")
        {
            return format!("git@{host}:{path}.git");
        }
        https_url.to_owned()
    }

    fn owner_repo(&self, url: &str) -> Option<(String, String)> {
        let (_, path) = gitlab_host_path(url)?;
        // Split at last '/' — GitLab paths may be multi-level (group/sub/repo).
        let (prefix, last) = path.rsplit_once('/')?;
        if prefix.is_empty() || last.is_empty() {
            return None;
        }
        Some((prefix.to_owned(), last.to_owned()))
    }

    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)> {
        // https://gitlab.com/group/subgroup/repo/-/merge_requests/42
        let after_host = url
            .strip_prefix("https://")
            .and_then(|s| s.split_once('/'))?
            .1;
        let (slug, num_part) = after_host.rsplit_once("/-/merge_requests/")?;
        let num: i64 = num_part.split(&['/', '?', '#'][..]).next()?.parse().ok()?;
        Some((slug.to_owned(), num))
    }

    fn create_pr(
        &self,
        repo_path: &Path,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        // Derive -R flag from the git remote in cwd (glab infers host).
        let title_arg = if title.is_empty() { head } else { title };
        let (rc, out) = run_cmd_cwd(
            &[
                "glab",
                "mr",
                "create",
                "--source-branch",
                head,
                "--target-branch",
                base,
                "--draft",
                "--title",
                title_arg,
                "--description",
                body,
                "--yes",
            ],
            repo_path,
            300,
        );
        if rc != 0 {
            anyhow::bail!("glab mr create failed (rc={rc}): {out}");
        }
        // glab prints the MR URL; search for it.
        for line in out.trim().lines().rev() {
            if line.contains("/-/merge_requests/") {
                return Ok(line.trim().to_owned());
            }
        }
        Ok(out.trim().lines().last().unwrap_or("").to_owned())
    }

    fn view_pr_sync(&self, url: &str, pr_number: i64) -> anyhow::Result<PrView> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let (mr, notes) = gitlab_fetch_mr(url, path, pr_number)?;
        let (comments, reviews) = gitlab_split_notes(&notes);
        let state = gitlab_norm_state(&mr.state);
        let mergeable = gitlab_norm_mergeable(&mr);
        let status_check_rollup = gitlab_norm_pipeline(&mr);
        Ok(PrView {
            state,
            mergeable,
            review_decision: ReviewDecision::None,
            head_ref: mr.source_branch,
            head_sha: mr.sha,
            updated_at: mr.updated_at,
            title: String::new(),
            body: String::new(),
            status_check_rollup,
            comments,
            reviews,
        })
    }

    fn view_pr_engage(&self, url: &str, pr_number: i64) -> anyhow::Result<PrView> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let (mr, notes) = gitlab_fetch_mr(url, path, pr_number)?;
        let (comments, reviews) = gitlab_split_notes(&notes);
        let state = gitlab_norm_state(&mr.state);
        let mergeable = gitlab_norm_mergeable(&mr);
        let status_check_rollup = gitlab_norm_pipeline(&mr);
        Ok(PrView {
            state,
            mergeable,
            review_decision: ReviewDecision::None,
            head_ref: mr.source_branch,
            head_sha: mr.sha,
            updated_at: mr.updated_at,
            title: mr.title,
            body: mr.description,
            status_check_rollup,
            comments,
            reviews,
        })
    }
    fn comment_pr(&self, url: &str, pr_number: i64, body: &str) -> anyhow::Result<()> {
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let enc = path.replace('/', "%2F");
        let api_path = format!("projects/{enc}/merge_requests/{pr_number}/notes");
        let (rc, out) = gitlab_api(url, &api_path, Some("POST"), &[("body", body)]);
        if rc != 0 {
            anyhow::bail!("glab api POST notes failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn close_pr(&self, url: &str, pr_number: i64, comment: &str) -> anyhow::Result<()> {
        // Close only after the withdrawal reason has landed.
        let truncated: String = comment.chars().take(800).collect();
        if !truncated.is_empty() {
            self.comment_pr(url, pr_number, &truncated)?;
        }
        let (_, path) = gitlab_host_path(url)
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitLab URL: {url}"))?;
        let repo_flag = if let Some((host, _)) = gitlab_host_path(url)
            && host != "gitlab.com"
        {
            format!("https://{host}/{path}")
        } else {
            path.to_owned()
        };
        let num = pr_number.to_string();
        let (rc, out) = run_cmd(&["glab", "mr", "close", &num, "-R", &repo_flag], 60);
        if rc != 0 {
            anyhow::bail!("glab mr close failed (rc={rc}): {out}");
        }
        Ok(())
    }

    fn push(&self, repo_path: &Path, url: &str, branch: &str) -> anyhow::Result<()> {
        let ssh = self.ssh_url(url);
        let refspec = format!("HEAD:{branch}");
        let (rc, out) = run_cmd_cwd(&["git", "push", "--force", &ssh, &refspec], repo_path, 120);
        if rc != 0 {
            anyhow::bail!("git push failed (rc={rc}): {out}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Factory (`forge.forge_for`, `forge.detect_forge`)
// ---------------------------------------------------------------------------

/// Factory: pick the right Forge from a repo's forge field.
pub fn forge_for(forge: ForgeName) -> Box<dyn Forge> {
    match forge {
        ForgeName::Gitlab => Box::new(GitLabForge),
        ForgeName::Github => Box::new(GitHubForge),
    }
}

/// Host of a git remote URL: `https://`, `http://`, `ssh://` and the
/// scp-like `git@host:path` form, with any `user@` and `:port` stripped.
///
/// One parser, because the alternative is what this replaced: a
/// substring test against the whole URL, which called a GitHub repo
/// named `gitlab-ci-templates` a GitLab repo.
pub fn url_host(url: &str) -> Option<&str> {
    let after_scheme = ["https://", "http://", "ssh://"]
        .iter()
        .find_map(|p| url.strip_prefix(p));
    let authority = match after_scheme {
        // scheme://[user@]host[:port]/path
        Some(rest) => rest.split('/').next()?,
        // scp-like: [user@]host:path — the colon separates the path, not
        // a port, so split there and not on ':'.
        None => url.split_once(':').map(|(a, _)| a)?,
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Only scheme URLs can carry a port; in the scp form the colon is
    // already gone with the path.
    let host = if after_scheme.is_some() {
        host.split_once(':').map_or(host, |(h, _)| h)
    } else {
        host
    };
    (!host.is_empty()).then_some(host)
}

/// Which forge serves `url`, from its host alone.
///
/// The asymmetry is the point: GitHub is only ever reachable at domains
/// GitHub itself operates — `github.com` and GitHub Enterprise Cloud's
/// `<org>.ghe.com` — plus Enterprise Server installs, which by
/// convention carry a `github` label (`github.corp.com`). GitLab has no
/// such bound: any hostname at all can be a self-hosted GitLab, and
/// `code.corp.com` or `git.corp.com` usually is one. So GitHub gets the
/// closed list and GitLab gets the remainder.
///
/// Matching is per label rather than by substring, so a host merely
/// containing the letters (`notgithub.com`) is not GitHub. That is a
/// sanity bound, not an anti-spoofing measure: the URL comes from the
/// operator adding their own repo, and `github.com.evil.example` would
/// still read as GitHub.
///
/// This is only the fallback. An operator who runs something else, or a
/// GitHub Enterprise Server at a host that hides it, passes `forge`
/// explicitly on POST /api/repos and never reaches this function.
pub fn detect_forge(url: &str) -> ForgeName {
    if is_github_host(url_host(url).unwrap_or_default()) {
        ForgeName::Github
    } else {
        ForgeName::Gitlab
    }
}

#[cfg(test)]
mod run_cmd_cwd_tests {
    use super::*;

    /// The working directory is the whole point of this wrapper.
    /// Deliberately not the test's own cwd: a delegation that dropped the
    /// directory would still pass against a path the process is in anyway.
    #[test]
    fn test_command_runs_in_the_requested_directory() {
        let (rc, out) = run_cmd_cwd(&["sh", "-c", "pwd -P"], Path::new("/"), 30);
        assert_eq!(rc, 0);
        assert_eq!(out.trim_end(), "/");
    }

    /// The delegation has to carry the process-group reap with it, not
    /// just the cwd. A child that exits promptly while leaving a
    /// descendant holding the inherited pipes hangs `join_pipes` forever
    /// on this path, where no deadline is in force to rescue it — the bug
    /// the copied loop here shipped for two rounds.
    #[test]
    fn test_prompt_exit_with_a_lingering_descendant_does_not_block() {
        let t0 = std::time::Instant::now();
        let (rc, _out) = run_cmd_cwd(&["sh", "-c", "sleep 30 & exit 0"], Path::new("/"), 60);
        let elapsed = t0.elapsed();

        assert_eq!(rc, 0, "the direct child exited cleanly");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "run_cmd_cwd blocked on a descendant holding the pipes: {elapsed:?}"
        );
    }
}
