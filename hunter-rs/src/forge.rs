//! Forge abstraction — GitHub / GitLab PR/MR lifecycle via CLI tools.
//! Port of hunter/forge.py. Methods shell out to `gh` / `glab`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::domain::ForgeName;
use crate::util::{drain_pipe, join_pipes, run_cmd};

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

/// Like [`run_cmd`] but runs the command in `cwd`. Needed because the
/// frozen `util::run_cmd` does not accept a working directory.
fn run_cmd_cwd(argv: &[&str], cwd: &Path, timeout_s: u64) -> (i32, String) {
    let Some((prog, rest)) = argv.split_first() else {
        return (127, "empty argv".to_owned());
    };
    let mut child = match Command::new(prog)
        .args(rest)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (127, e.to_string()),
    };
    // Drain both pipes while the child runs: reading them only after it
    // exits deadlocks as soon as the child fills a pipe buffer.
    let so = drain_pipe(child.stdout.take());
    let se = drain_pipe(child.stderr.take());
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), join_pipes(so, se)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let out = join_pipes(so, se);
                    return (124, format!("timeout after {timeout_s}s\n{out}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let out = join_pipes(so, se);
                return (127, format!("{e}\n{out}"));
            }
        }
    }
}

/// Parse owner/repo from a GitHub-style URL (HTTPS or SSH).
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_owned(), repo.to_owned()))
}

/// Extract (host, `project_path`) from a GitLab-style URL.
fn gitlab_host_path(url: &str) -> Option<(&str, &str)> {
    if let Some(rest) = url.strip_prefix("https://") {
        let slash = rest.find('/')?;
        let host = &rest[..slash];
        let path = rest[slash + 1..]
            .trim_end_matches('/')
            .trim_end_matches(".git");
        if path.is_empty() {
            return None;
        }
        return Some((host, path));
    }
    if let Some(rest) = url.strip_prefix("git@") {
        let colon = rest.find(':')?;
        let host = &rest[..colon];
        let path = rest[colon + 1..]
            .trim_end_matches('/')
            .trim_end_matches(".git");
        if path.is_empty() {
            return None;
        }
        return Some((host, path));
    }
    None
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
// GitLab normalisation helpers (forge.py:294-343)
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
// GitHub (via `gh` CLI) — forge.py:95-219
// ---------------------------------------------------------------------------

pub struct GitHubForge;
pub struct GitLabForge;

impl Forge for GitHubForge {
    fn ssh_url(&self, https_url: &str) -> String {
        if let Some(rest) = https_url.strip_prefix("https://github.com/") {
            let path = rest.trim_end_matches('/').trim_end_matches(".git");
            if !path.is_empty() {
                return format!("git@github.com:{path}.git");
            }
        }
        https_url.to_owned()
    }

    fn owner_repo(&self, url: &str) -> Option<(String, String)> {
        github_owner_repo(url)
    }

    fn parse_pr_url(&self, url: &str) -> Option<(String, i64)> {
        // https://github.com/owner/repo/pull/123
        let path = url.strip_prefix("https://github.com/")?;
        let (slug, num_part) = path.rsplit_once("/pull/")?;
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
// GitLab (via `glab` CLI + REST API) — forge.py:227-487
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
// Factory (forge.py:490-527)
// ---------------------------------------------------------------------------

/// Factory: pick the right Forge from a repo's forge field.
pub fn forge_for(forge: ForgeName) -> Box<dyn Forge> {
    match forge {
        ForgeName::Gitlab => Box::new(GitLabForge),
        ForgeName::Github => Box::new(GitHubForge),
    }
}

/// Best-effort forge type from a remote URL host.
pub fn detect_forge(url: &str) -> ForgeName {
    if url.contains("gitlab") {
        ForgeName::Gitlab
    } else {
        ForgeName::Github
    }
}
