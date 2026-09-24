//! Scheduler — job selection and the executors that carry it out.
//!
//! The selection half (`pick_next`, `anticipated_tokens`) is also what
//! `GET /api/summary` uses for its next-candidate preview, so the
//! endpoint and the loop cannot disagree about what runs next. The
//! `run_*` executors below are the other half.

use std::path::Path;

use crate::config::Config;
use crate::domain::{FindingJobKind, FindingStatus, FindingType, JobKind, JobState, RepoJobKind};
use crate::store::{FindingFilter, Store, StoreWriteError, SyncPrData};
use crate::types::{Finding, Repo};
use crate::util::now_ms;

/// What `run_cycle` would act on right now, if invoked (scheduler.py
/// `pick_next` docstring). Enough for the summary preview and for
/// `anticipated_tokens(repo_id`, kind).
#[derive(Debug, Clone)]
pub enum Candidate {
    Repo {
        kind: RepoJobKind,
        repo_id: i64,
        label: Option<String>,
    },
    Finding {
        kind: FindingJobKind,
        finding_id: i64,
        repo_id: i64,
        label: Option<String>,
        budget_override: Option<String>,
    },
}

impl Candidate {
    pub fn job_kind(&self) -> JobKind {
        match self {
            Self::Repo { kind, .. } => (*kind).into(),
            Self::Finding { kind, .. } => (*kind).into(),
        }
    }

    pub fn repo_id(&self) -> i64 {
        match self {
            Self::Repo { repo_id, .. } | Self::Finding { repo_id, .. } => *repo_id,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Repo { label, .. } | Self::Finding { label, .. } => label.as_deref(),
        }
    }

    /// The primary target ID: `finding_id` for finding kinds, `repo_id` for repo kinds.
    pub fn target_id(&self) -> i64 {
        match self {
            Self::Repo { repo_id, .. } => *repo_id,
            Self::Finding { finding_id, .. } => *finding_id,
        }
    }

    pub fn budget_override(&self) -> Option<&str> {
        match self {
            Self::Finding {
                budget_override, ..
            } => budget_override.as_deref(),
            Self::Repo { .. } => None,
        }
    }
}

/// Python's truthiness on `budget_override`: NULL and "" are both "no
/// override" (`if f.get("budget_override")` in `scheduler.pick_next`).
fn override_of(f: &Finding) -> Option<&str> {
    f.budget_override.as_deref().filter(|s| !s.is_empty())
}

/// Candidate for a finding-target kind (engage/harvest/recheck/fix).
/// label = summary || fingerprint under Python `or` semantics (findings
/// carry no "name" key, the `next_candidate` label in
/// `server.Handler._summary`): an empty summary falls through.
fn finding_candidate(kind: FindingJobKind, f: &Finding) -> Candidate {
    let label = if f.summary.is_empty() {
        f.fingerprint.clone()
    } else {
        f.summary.clone()
    };
    Candidate::Finding {
        kind,
        finding_id: f.id,
        repo_id: f.repo_id,
        label: Some(label),
        budget_override: override_of(f).map(str::to_owned),
    }
}

/// Candidate for a repo-target kind (`hunt/test_gap/dep_update/refactor`/
/// modernization/standards). label = name (repos carry neither "summary"
/// nor "fingerprint").
fn repo_candidate(kind: RepoJobKind, r: &Repo) -> Candidate {
    Candidate::Repo {
        kind,
        repo_id: r.id,
        label: (!r.name.is_empty()).then(|| r.name.clone()),
    }
}

async fn findings_with_status(store: &Store, status: FindingStatus) -> sqlx::Result<Vec<Finding>> {
    store
        .list_findings(&FindingFilter {
            status: Some(status),
            ..FindingFilter::default()
        })
        .await
}

/// store.py `list_attention`: `pr_open` findings whose `pr_state` row has
/// `needs_attention` IS NOT NULL, ORDER BY `COALESCE(attention_since`,
/// `synced_at`) ascending — oldest-outstanding reason first, NULL first
/// (SQLite NULL-first ASC == Option's None < Some). Composed from the
/// frozen `list_findings` + `get_pr_state` instead of a bespoke JOIN; the
/// JOIN's row-presence requirement is implied by `needs_attention` being
/// non-NULL.
async fn list_attention(store: &Store) -> sqlx::Result<Vec<Finding>> {
    let mut rows: Vec<(Option<i64>, Finding)> = Vec::new();
    for f in findings_with_status(store, FindingStatus::PrOpen).await? {
        if let Some(ps) = store.get_pr_state(f.id).await?
            && ps.needs_attention.is_some()
        {
            rows.push((ps.attention_since.or(ps.synced_at), f));
        }
    }
    rows.sort_by_key(|(key, _)| *key); // stable, like SQLite's unspecified tie order
    Ok(rows.into_iter().map(|(_, f)| f).collect())
}

/// store.py `list_pending_harvest`: merged findings whose `pr_state` row has
/// `harvested_at` IS NULL, ORDER BY `synced_at` ascending (oldest merge
/// first). The JOIN requires the `pr_state` row to exist.
async fn list_pending_harvest(store: &Store) -> sqlx::Result<Vec<Finding>> {
    let mut rows: Vec<(Option<i64>, Finding)> = Vec::new();
    for f in findings_with_status(store, FindingStatus::Merged).await? {
        if let Some(ps) = store.get_pr_state(f.id).await?
            && ps.harvested_at.is_none()
        {
            rows.push((ps.synced_at, f));
        }
    }
    rows.sort_by_key(|(key, _)| *key);
    Ok(rows.into_iter().map(|(_, f)| f).collect())
}

/// Pure selection — replicates `scheduler.pick_next`'s priority order and
/// per-kind eligibility conditions. Read-only.
///
/// Priority (`scheduler.pick_next`): a budget-overridden finding (any
/// category) jumps the queue -> flagged PR (oldest-outstanding reason
/// first) -> oldest merged PR pending follow-up review -> oldest
/// rechecking -> oldest queued fix -> the most stale-of-rotation job
/// type for the least-recently-hunted enabled repo (hunt if never
/// cloned, else whichever of `hunt/test_gap/dep_update/refactor` is
/// oldest/never-run subject to `cfg.scan_interval_days`; modernization
/// gated by `cfg.modernization_interval_days`). All types gated for one
/// repo -> the next-stalest repo is tried. None = nothing to do.
pub async fn pick_next(
    store: &Store,
    cfg: &Config,
    force_repo: Option<&str>,
) -> anyhow::Result<Option<Candidate>> {
    let mut rechecking = findings_with_status(store, FindingStatus::Rechecking).await?;
    let mut attention = list_attention(store).await?;
    let mut pending_harvest = list_pending_harvest(store).await?;
    let mut queued = findings_with_status(store, FindingStatus::Queued).await?;

    let force_id: Option<i64> = if let Some(name) = force_repo {
        let r = store
            .get_repo_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown repo {name:?}"))?;
        Some(r.id)
    } else {
        None
    };

    if let Some(rid) = force_id {
        rechecking.retain(|f| f.repo_id == rid);
        attention.retain(|f| f.repo_id == rid);
        pending_harvest.retain(|f| f.repo_id == rid);
        queued.retain(|f| f.repo_id == rid);
    }

    // (1) A budget-overridden finding jumps the queue, scanned in the
    // same kind order the normal priorities use (`scheduler.pick_next`).
    for (kind, items) in [
        (FindingJobKind::Engage, &attention),
        (FindingJobKind::Harvest, &pending_harvest),
        (FindingJobKind::Recheck, &rechecking),
        (FindingJobKind::Fix, &queued),
    ] {
        if let Some(f) = items.iter().find(|f| override_of(f).is_some()) {
            return Ok(Some(finding_candidate(kind, f)));
        }
    }

    // (2)-(5) Normal finding-kind priorities. attention/pending_harvest
    // are oldest-first; list_findings is id DESC, so last = oldest.
    if let Some(f) = attention.first() {
        return Ok(Some(finding_candidate(FindingJobKind::Engage, f)));
    }
    if let Some(f) = pending_harvest.first() {
        return Ok(Some(finding_candidate(FindingJobKind::Harvest, f)));
    }
    if let Some(f) = rechecking.last() {
        return Ok(Some(finding_candidate(FindingJobKind::Recheck, f)));
    }
    if let Some(f) = queued.last() {
        return Ok(Some(finding_candidate(FindingJobKind::Fix, f)));
    }

    // (6) Repo rotation: enabled repos in staleness order — never-hunted
    // first, then oldest last_hunt_at first; ties keep list_repos' name
    // order (Python sorted() and Vec::sort_by_key are both stable).
    let mut repos: Vec<Repo> = store
        .list_repos()
        .await?
        .into_iter()
        .filter(|r| {
            if let Some(rid) = force_id {
                r.id == rid
            } else {
                r.enabled != 0
            }
        })
        .collect();
    repos.sort_by_key(|r| (r.last_hunt_at.is_some(), r.last_hunt_at.unwrap_or(0)));

    let scan_interval_ms = (cfg.scan_interval_days * 86_400_000.0) as i64;
    let mod_interval_ms = cfg.modernization_interval_days * 86_400_000;
    let now = now_ms();

    for target in &repos {
        if !Path::new(&target.path).exists() {
            // Not cloned yet -> hunt does the clone (`scheduler.pick_next`).
            return Ok(Some(repo_candidate(RepoJobKind::Hunt, target)));
        }

        // Eligible job types with their last-run timestamps, in
        // RepoJobKind::ALL insertion order (mirrors Python's dict).
        let mut job_times: Vec<(RepoJobKind, i64)> = Vec::with_capacity(RepoJobKind::ALL.len());
        for (kind, last) in [
            (RepoJobKind::Hunt, target.last_hunt_at),
            (RepoJobKind::TestGap, target.last_test_gap_at),
            (RepoJobKind::DepUpdate, target.last_dep_update_at),
            (RepoJobKind::Refactor, target.last_refactor_at),
        ] {
            let last = last.unwrap_or(0);
            if last == 0 || (now - last) >= scan_interval_ms {
                job_times.push((kind, last));
            }
        }
        let last_modernization = target.last_modernization_at.unwrap_or(0);
        if last_modernization == 0 || (now - last_modernization) >= mod_interval_ms {
            job_times.push((RepoJobKind::Modernization, last_modernization));
        }
        let last_standards = target.last_standards_at.unwrap_or(0);
        let std_interval_ms = cfg.standards_interval_days * 86_400_000;
        if last_standards == 0 || (now - last_standards) >= std_interval_ms {
            job_times.push((RepoJobKind::Standards, last_standards));
        }

        if job_times.is_empty() {
            continue; // all scan types ran within their intervals for this repo
        }

        // never-run beats stale-run; both tie-break on JOB_TYPE_PRIORITY,
        // which is job_times' insertion order (Python: next() over
        // _JOB_TYPE_PRIORITY, resp. min() first-wins in insertion order —
        // Iterator::min_by_key also returns the FIRST minimal element).
        let picked = job_times
            .iter()
            .find(|&&(_, last)| last == 0)
            .or_else(|| job_times.iter().min_by_key(|&&(_, last)| last));
        if let Some(&(job_type, _)) = picked {
            return Ok(Some(repo_candidate(job_type, target)));
        }
    }

    Ok(None)
}

/// Historical cost estimate for one (repo, kind): warm (finished a
/// non-denied job of this kind on this repo within `cache_ttl_s`) -> p50 of
/// the kind's `tokens_new` history, cold -> p90; empty history -> 0
/// (`scheduler.anticipated_tokens`, exact index formula in
/// BACKEND-CONTRACT.md §1.9; the history itself is
/// `Store::kind_token_history`).
pub async fn anticipated_tokens(
    store: &Store,
    cfg: &Config,
    repo_id: i64,
    kind: JobKind,
) -> anyhow::Result<i64> {
    let cache_ttl_ms = (cfg.cache_ttl_s * 1000.0) as i64;
    let cutoff = now_ms() - cache_ttl_ms;
    let warm = store.has_warm_job(repo_id, kind.as_str(), cutoff).await?;
    let history = store.kind_token_history(kind.as_str()).await?;
    if history.is_empty() {
        return Ok(0);
    }
    let frac = if warm { 0.5 } else { 0.9 };
    let idx = ((history.len() as f64 * frac) as usize).min(history.len() - 1);
    Ok(history[idx])
}

// ---------------------------------------------------------------------------
// Executor functions: one per job kind, ported from scheduler.py
// ---------------------------------------------------------------------------

use std::path::PathBuf;

use crate::backend::{Backend, JobClass, Verdict};
use crate::forge::{self, CheckConclusion, GhCheckRun, Mergeable, PrView, ReviewDecision};
use crate::ingest::{IngestResult, ingest_findings};
use crate::playbooks;
use crate::types::RunResult;
use serde::{Deserialize, Serialize};

/// Sync PR results — typed replacement for raw JSON blobs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncResult {
    pub synced: i64,
    pub merged: i64,
    pub closed: i64,
    pub attention: i64,
    pub errors: i64,
}

/// Cycle summary — typed replacement for raw JSON returned by all
/// run_* functions. Fields are Option so each runner sets only what it needs.
/// Serialize skips None fields for clean JSON tracing output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CycleSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<JobKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(rename = "finding", skip_serializing_if = "Option::is_none")]
    pub finding_id: Option<i64>,
    #[serde(rename = "job", skip_serializing_if = "Option::is_none")]
    pub job_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<JobState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_new: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_rehunt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingest: Option<IngestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(rename = "pr", skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
}

/// Maximum consecutive identical failures before giving up (`scheduler.MAX_CONSECUTIVE_SAME_FAILURE`).
const MAX_CONSECUTIVE_SAME_FAILURE: i64 = 3;

/// Map `RunResult` to job state.
fn job_state(rr: &RunResult) -> JobState {
    if rr.killed_reason.is_some() {
        JobState::Killed
    } else if rr.exit_code == Some(0) {
        JobState::Done
    } else {
        JobState::Failed
    }
}

/// Turn a refused `create_job` into a skipped cycle.
///
/// The repo was live when this run was picked and soft-deleted before the
/// job row went in. Losing that race is ordinary operator traffic, not a
/// fault: reporting it as an error would log "cycle crashed" and hand the
/// operator a stack trace for having deleted a repo. A genuine DB failure
/// still propagates.
fn job_refused(
    kind: JobKind,
    repo: Option<&str>,
    finding_id: Option<i64>,
    err: StoreWriteError,
) -> anyhow::Result<CycleSummary> {
    match err {
        StoreWriteError::Refused(msg) => Ok(CycleSummary {
            kind: Some(kind),
            repo: repo.map(ToOwned::to_owned),
            finding_id,
            skipped: Some(msg),
            ..Default::default()
        }),
        StoreWriteError::Db(e) => Err(e.into()),
    }
}

/// Record a completed job's outcome (`scheduler._record_job`).
pub async fn record_job(
    store: &Store,
    job_id: i64,
    rr: &RunResult,
    model: Option<&str>,
) -> anyhow::Result<JobState> {
    let state = job_state(rr);
    let notes = if state != JobState::Done && !rr.stdout_tail.is_empty() {
        Some(crate::util::tail(&rr.stdout_tail, 500))
    } else {
        None
    };
    let exit_code = rr.exit_code.map(i64::from);
    let now = now_ms();
    store
        .complete_job(
            job_id,
            state,
            rr.tokens_new,
            rr.calls,
            exit_code,
            rr.killed_reason.as_deref(),
            rr.session_file.as_deref(),
            notes,
            model,
            rr.usage_delta,
            now,
        )
        .await?;
    Ok(state)
}

/// Ingest a worker's `FOLLOW-UPS.json`, returning what happened so the
/// caller can put it in the cycle summary. Rejections used to be visible
/// only as per-entry `ingest:` error events — the summary carried no
/// `ingest` block and the event below fired only when something was
/// inserted, so a harvest whose every follow-up was rejected logged
/// "harvested" and nothing else (observed 2026-09-12 and 2026-09-22:
/// four `test_gap` follow-ups silently dropped).
async fn ingest_followups(
    store: &Store,
    repo_id: i64,
    worktree: &Path,
    fid: i64,
    job: i64,
    kind: &str,
) -> Option<crate::ingest::IngestResult> {
    let followups_path = worktree.join("FOLLOW-UPS.json");
    if !followups_path.exists() {
        return None;
    }
    let counts = ingest_findings(store, repo_id, &followups_path, None, Some(job), Some(fid)).await;
    if counts.inserted > 0 || counts.invalid > 0 || counts.duplicates > 0 {
        let _ = store
            .log_event(
                kind,
                &format!(
                    "#{fid}: +{} follow-up(s) filed from deferred/superseded work ({} dup / {} invalid)",
                    counts.inserted, counts.duplicates, counts.invalid
                ),
                Some(job),
                Some(fid),
            )
            .await;
    }
    Some(counts)
}

/// Blocking git command wrapper for use inside `spawn_blocking`.
fn run_cmd_sync(argv: &[&str], timeout_s: u64) -> (i32, String) {
    crate::util::run_cmd(argv, timeout_s)
}

/// Clone repo if not yet cloned, then fetch+checkout+pull default branch.
/// Returns Ok(()) on success, Err with an error summary dict on failure.
async fn sync_repo(
    store: &Store,
    repo_url: &str,
    rpath: &Path,
    default_branch: &str,
    log_prefix: &str,
    finding_id: Option<i64>,
) -> Result<(), String> {
    if rpath.exists() {
        // The directory already exists, so the clone is skipped -- which is
        // only safe if it is a clone of *this* repo. `repos/repo-<id>` is
        // derived from the id, so a directory a failed reclamation left
        // behind would otherwise be hunted under a later repo's identity
        // and pushed to that repo's URL.
        // Deletion now removes the directory, so reaching here means
        // something outside the daemon put it there: refuse rather than
        // guess.
        let rp = rpath.to_string_lossy().to_string();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(&["git", "-C", &rp, "remote", "get-url", "origin"], 60)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        let origin = out.trim();
        if rc != 0 || origin != repo_url {
            let msg = format!(
                "{} exists but its origin is {} -- expected {repo_url}; \
                 refusing to work in a clone of a different repository",
                rpath.display(),
                if rc == 0 { origin } else { "unreadable" }
            );
            let _ = store
                .log_event("error", &format!("{log_prefix}: {msg}"), None, finding_id)
                .await;
            return Err(msg);
        }
    } else {
        if let Some(parent) = rpath.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let url = repo_url.to_owned();
        let rp = rpath.to_owned();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            // `--` before the operands: even a URL that slipped past
            // validation cannot be read as an option here.
            run_cmd_sync(&["git", "clone", "--", &url, &rp.to_string_lossy()], 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let _ = store
                .log_event(
                    "error",
                    &format!("{log_prefix}: clone failed: {tail}"),
                    None,
                    finding_id,
                )
                .await;
            return Err(format!("clone failed: {tail}"));
        }
    }
    let rp_str = rpath.to_string_lossy().to_string();
    for cmd_tail in [
        vec!["fetch".to_owned(), "origin".to_owned()],
        vec!["checkout".to_owned(), default_branch.to_owned()],
        vec!["pull".to_owned(), "--ff-only".to_owned()],
    ] {
        let rps = rp_str.clone();
        let ct = cmd_tail.clone();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            let mut argv: Vec<&str> = vec!["git", "-C", &rps];
            argv.extend(ct.iter().map(std::string::String::as_str));
            run_cmd_sync(&argv, 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let cmd_str = format!("git {}", cmd_tail.join(" "));
            let _ = store
                .log_event(
                    "error",
                    &format!("{log_prefix}: {cmd_str} failed: {tail}"),
                    None,
                    finding_id,
                )
                .await;
            return Err(format!("{cmd_str} failed: {tail}"));
        }
    }
    Ok(())
}

/// Budget gate: check with backend, return the grant or a denied summary.
enum BudgetDecision {
    /// The cap to enforce, and the anticipated cost the ramp reserved to
    /// grant it — the job row records the latter so the inflight
    /// reservation can read it back while the job runs.
    Approved {
        cap: i64,
        anticipated: i64,
    },
    Denied(Box<CycleSummary>),
}

async fn budget_gate(
    backend: &dyn Backend,
    store: &Store,
    cfg: &Config,
    repo_id: i64,
    kind: JobKind,
    cfg_cap: i64,
    use_override: bool,
    log_prefix: &str,
    finding_id: Option<i64>,
) -> anyhow::Result<BudgetDecision> {
    let anticipated = anticipated_tokens(store, cfg, repo_id, kind)
        .await
        .unwrap_or(0);
    let outlook = backend.decide(anticipated).await?;
    let verdict = if use_override {
        &outlook.prioritized
    } else {
        &outlook.normal
    };
    match verdict {
        Verdict::Denied { reason, retry_at } => {
            let _ = store
                .log_event("deny", &format!("{log_prefix}: {reason}"), None, finding_id)
                .await;
            Ok(BudgetDecision::Denied(Box::new(CycleSummary {
                kind: Some(kind),
                denied: Some(reason.clone()),
                retry_at: retry_at.as_ref().map(|v| *v as i64),
                ..Default::default()
            })))
        }
        Verdict::Granted {
            cap_tokens: backend_cap,
            ..
        } => {
            let cap = match backend_cap {
                Some(bc) => cfg_cap.min(*bc),
                None => cfg_cap,
            };
            Ok(BudgetDecision::Approved { cap, anticipated })
        }
    }
}

/// Run a hunt job (`scheduler.run_hunt`).
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: sync, resolve diff range, gate budget, \
              run the worker, ingest. The steps share a dozen locals and \
              each early-returns a `CycleSummary`, so helper extraction \
              would only move the same state behind argument lists"
)]
pub async fn run_hunt(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    // Git's empty tree — the implicit parent of all root commits.
    // Using this as diff base includes the root commit itself.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
    let db = &repo.default_branch;

    sync_repo(store, &repo.url, &rpath, db, &format!("hunt {rname}"), None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Get HEAD
    let rp_str = rpath.to_string_lossy().to_string();
    let (rc, head) = {
        let rps = rp_str.clone();
        tokio::task::spawn_blocking(move || {
            run_cmd_sync(&["git", "-C", &rps, "rev-parse", "HEAD"], 30)
        })
        .await
        .unwrap_or((127, String::new()))
    };
    let head = head.trim().to_owned();
    if rc != 0 || head.is_empty() {
        let _ = store
            .log_event(
                "error",
                &format!("hunt {rname}: rev-parse HEAD failed"),
                None,
                None,
            )
            .await;
        anyhow::bail!("rev-parse HEAD failed");
    }

    let last = repo
        .last_hunt_sha
        .as_deref()
        .map(std::borrow::ToOwned::to_owned);
    let last_full = repo.last_full_hunt_at;
    let rehunt_interval_ms = cfg.hunt_rehunt_days * 86_400_000;
    let rehunt_due = last_full.is_some_and(|lf| (now_ms() - lf) > rehunt_interval_ms);
    let mut full_rehunt_triggered = false;
    let last = if rehunt_due {
        let _ = store.clear_last_hunt_sha(rid).await;
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: full re-hunt triggered ({}d interval)",
                    cfg.hunt_rehunt_days
                ),
                None,
                None,
            )
            .await;
        full_rehunt_triggered = true;
        None
    } else {
        last
    };
    if last.as_deref() == Some(&head) {
        let _ = store.set_last_hunt(rid, &head).await;
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: no new commits since {} -- skipped",
                    &head[..12.min(head.len())]
                ),
                None,
                None,
            )
            .await;
        return Ok(CycleSummary {
            kind: Some(RepoJobKind::Hunt.into()),
            skipped: Some("no new commits".into()),
            head: Some(head),
            ..Default::default()
        });
    }
    let (diff_range, scope_note) = if let Some(ref l) = last {
        // Incremental: only commits since last hunt
        (
            format!("{l}..{head}"),
            format!(
                "Commits since the last completed hunt ({}).",
                &l[..12.min(l.len())]
            ),
        )
    } else if full_rehunt_triggered {
        // Full re-hunt: complete history including root commit
        (
            format!("{EMPTY_TREE}..{head}"),
            "Periodic full re-hunt: complete history including root commit.".to_owned(),
        )
    } else {
        // First hunt: last 3 weeks or 30 commits (bounded)
        let rps = rp_str.clone();
        let h = head.clone();
        let (_rc, base) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &rps,
                    "rev-list",
                    "-1",
                    "--before=3 weeks ago",
                    &h,
                ],
                30,
            )
        })
        .await
        .unwrap_or((127, String::new()));
        let base = base.trim().to_owned();
        if base.is_empty() || base == head {
            let rps = rp_str.clone();
            let h = head.clone();
            let (rc2, base2) = tokio::task::spawn_blocking(move || {
                run_cmd_sync(&["git", "-C", &rps, "rev-parse", &format!("{h}~30")], 30)
            })
            .await
            .unwrap_or((127, String::new()));
            let base2 = base2.trim().to_owned();
            if rc2 != 0 || base2.is_empty() {
                // Very small repo — scan from root
                (
                    format!("{EMPTY_TREE}..{head}"),
                    "First hunt for this repo: the full history (small repo).".to_owned(),
                )
            } else {
                (
                    format!("{base2}..{head}"),
                    format!(
                        "First hunt for this repo: the last 30 commits (base {}).",
                        &base2[..12.min(base2.len())]
                    ),
                )
            }
        } else {
            (
                format!("{base}..{head}"),
                format!(
                    "First hunt for this repo: the last ~3 weeks (base {}).",
                    &base[..12.min(base.len())]
                ),
            )
        }
    };

    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        rid,
        RepoJobKind::Hunt.into(),
        cfg.hunt_cap_tokens,
        false,
        &format!("hunt {rname}"),
        None,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let job = match store
        .create_job(
            RepoJobKind::Hunt.into(),
            rid,
            None,
            cap,
            JobState::Running,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(RepoJobKind::Hunt.into(), Some(rname), None, e),
    };
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("job{job}.findings.json"));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));

    let suppressions = store
        .suppressions(rid, FindingType::Bug.as_str())
        .await
        .unwrap_or_default();
    let known = store
        .known_active(rid, FindingType::Bug.as_str())
        .await
        .unwrap_or_default();
    let repo_notes = Store::repo_notes(&cfg.work_root, rid);
    let prompt = playbooks::build_hunt_prompt(
        &cfg.root,
        repo,
        &diff_range,
        &scope_note,
        &suppressions,
        &known,
        &out_path,
        cfg.hunt_max_findings,
        &repo_notes,
    )?;
    let model = cfg.model_for("hunt");
    let rr = backend
        .run(&rpath, &prompt, cap, cfg.hunt_max_wall_s, JobClass::Hunt)
        .await?;
    let state = record_job(store, job, &rr, model)
        .await
        .unwrap_or(JobState::Failed);

    let mut summary = CycleSummary {
        kind: Some(RepoJobKind::Hunt.into()),
        repo: Some(rname.to_owned()),
        job_id: Some(job),
        state: Some(state),
        diff_range: Some(diff_range.clone()),
        tokens_new: Some(rr.tokens_new),
        head: Some(head.clone()),
        full_rehunt: Some(full_rehunt_triggered),
        ..Default::default()
    };

    if out_path.exists() {
        let counts = ingest_findings(
            store,
            rid,
            &out_path,
            Some(FindingType::Bug),
            Some(job),
            None,
        )
        .await;
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: job {job} {state} over {}... +{} new / {} dup / {} invalid ({} tok)",
                    &diff_range[..25.min(diff_range.len())],
                    counts.inserted,
                    counts.duplicates,
                    counts.invalid,
                    rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
        if state == JobState::Done && counts.invalid == 0 {
            let _ = store.set_last_hunt(rid, &head).await;
            if full_rehunt_triggered || last_full.is_none() {
                let _ = store.set_last_full_hunt(rid).await;
            }
        }
        summary.ingest = Some(counts);
    } else {
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: job {job} {state}, no findings file ({} tok)",
                    rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
    }
    Ok(summary)
}

/// Handle a recheck that failed to produce a valid verdict.
async fn handle_recheck_failure(
    store: &Store,
    summary: &mut CycleSummary,
    fid: i64,
    job: i64,
    failure: &str,
    override_mode: Option<&str>,
) -> CycleSummary {
    let streak = store
        .record_recheck_attempt(fid, failure)
        .await
        .unwrap_or(1);
    if streak >= MAX_CONSECUTIVE_SAME_FAILURE {
        let _ = store.set_finding_status(fid, FindingStatus::New).await;
        let _ = store.clear_recheck_attempts(fid).await;
        let _ = store
            .log_event(
                "recheck",
                &format!("#{fid} gave up after {streak} identical failures ({failure}); back to inbox for human triage"),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("stuck".into());
    } else {
        let _ = store
            .log_event(
                "recheck",
                &format!("#{fid}: job {job} {failure} -- will retry"),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("requeued".into());
    }
    if override_mode == Some("once") {
        let _ = store.set_budget_override(fid, None).await;
    }
    summary.clone()
}

/// Run a recheck job (`scheduler.run_recheck`).
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline whose verdict dispatch is a single match \
              over the worker's reply; every arm needs the same surrounding \
              locals (fid, job, summary, override_mode)"
)]
pub async fn run_recheck(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    #[derive(Debug, Clone)]
    enum RecheckOutcome {
        Confirmed,
        Stale,
        Invalid,
        Unknown(String),
    }

    impl<'de> serde::Deserialize<'de> for RecheckOutcome {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let s = String::deserialize(d)?;
            Ok(match s.as_str() {
                "confirmed" => Self::Confirmed,
                "stale" => Self::Stale,
                "invalid" => Self::Invalid,
                _ => Self::Unknown(s),
            })
        }
    }

    #[derive(Debug, Default, Deserialize)]
    struct RecheckVerdict {
        #[serde(default)]
        verdict: Option<RecheckOutcome>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        updated_summary: Option<String>,
        #[serde(default)]
        updated_detail: Option<String>,
        #[serde(default)]
        updated_confidence: Option<f64>,
        #[serde(default)]
        updated_severity: Option<String>,
    }

    let fid = finding.id;
    if finding.status != FindingStatus::Rechecking {
        return Ok(CycleSummary {
            kind: Some(FindingJobKind::Recheck.into()),
            skipped: Some(format!(
                "finding #{fid} is {}, not rechecking",
                finding.status
            )),
            ..Default::default()
        });
    }
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("recheck #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let rpath = PathBuf::from(&repo.path);
    sync_repo(
        store,
        &repo.url,
        &rpath,
        &repo.default_branch,
        &format!("recheck #{fid}"),
        Some(fid),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Recheck.into(),
        cfg.hunt_cap_tokens,
        override_mode.is_some(),
        &format!("recheck #{fid}"),
        Some(fid),
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let job = match store
        .create_job(
            FindingJobKind::Recheck.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(
                FindingJobKind::Recheck.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };
    let out_path = cfg.work_root.join("out").join(format!("recheck{fid}.json"));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));
    // Remove stale output from a previous crashed attempt — a leftover verdict
    // file would be read as this attempt's result.
    let _ = std::fs::remove_file(&out_path);

    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt =
        playbooks::build_recheck_prompt(&cfg.root, finding, &repo, &out_path, &repo_notes)?;
    let model = cfg.model_for("hunt");
    let rr = backend
        .run(&rpath, &prompt, cap, cfg.hunt_max_wall_s, JobClass::Hunt)
        .await?;
    let state = record_job(store, job, &rr, model)
        .await
        .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Recheck.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };

    // Parse verdict file — only read if the worker completed successfully.
    // A stale file from a crashed previous attempt was already deleted above;
    // this gate prevents reading partial output from a killed/failed worker.
    let verdict: Option<RecheckVerdict> = if state == JobState::Done && out_path.exists() {
        std::fs::read_to_string(&out_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
    } else {
        None
    };
    // Four failure cases preserved through the type structure:
    // 1. worker didn't complete  2. no verdict file  3. unparseable JSON  → verdict is None
    // 4. unrecognised verdict value → verdict.verdict is Some(Unknown(s))
    // 5. missing verdict field → verdict.verdict is None
    let Some(ref verdict_obj) = verdict else {
        let failure = if state != JobState::Done {
            format!("worker {state}")
        } else if !out_path.exists() {
            "no verdict file".to_owned()
        } else {
            "unparseable verdict file".to_owned()
        };
        return Ok(
            handle_recheck_failure(store, &mut summary, fid, job, &failure, override_mode).await,
        );
    };
    let Some(ref outcome) = verdict_obj.verdict else {
        return Ok(handle_recheck_failure(
            store,
            &mut summary,
            fid,
            job,
            "missing verdict field",
            override_mode,
        )
        .await);
    };

    let reason_owned = verdict_obj.reason.as_deref().unwrap_or_default();
    let reason: String = reason_owned.chars().take(500).collect();
    let reason = reason.as_str();

    let verdict_str = match outcome {
        RecheckOutcome::Confirmed => {
            let update = crate::store::FindingAnalysisUpdate {
                summary: verdict_obj.updated_summary.clone(),
                detail: verdict_obj.updated_detail.clone(),
                confidence: verdict_obj.updated_confidence,
                severity: verdict_obj.updated_severity.clone(),
            };
            let _ = store.update_finding_analysis(fid, &update).await;
            let _ = store.set_finding_status(fid, FindingStatus::New).await;
            let _ = store.clear_recheck_attempts(fid).await;
            let _ = store
                .log_event(
                    "recheck",
                    &format!("#{fid} confirmed: {reason}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("confirmed".into());
            "confirmed"
        }
        RecheckOutcome::Stale => {
            let _ = store
                .set_finding_verdict(fid, FindingStatus::Wontfix, &format!("recheck: {reason}"))
                .await;
            let _ = store.clear_recheck_attempts(fid).await;
            let _ = store
                .log_event(
                    "recheck",
                    &format!("#{fid} stale: {reason}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("stale".into());
            "stale"
        }
        RecheckOutcome::Invalid => {
            let _ = store
                .set_finding_verdict(fid, FindingStatus::Rejected, &format!("recheck: {reason}"))
                .await;
            let _ = store.clear_recheck_attempts(fid).await;
            let _ = store
                .log_event(
                    "recheck",
                    &format!("#{fid} invalid: {reason}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("invalid".into());
            "invalid"
        }
        // Unknown already handled above — included for exhaustiveness
        RecheckOutcome::Unknown(raw) => {
            let failure = format!("invalid verdict value: {raw:?}");
            return Ok(handle_recheck_failure(
                store,
                &mut summary,
                fid,
                job,
                &failure,
                override_mode,
            )
            .await);
        }
    };
    summary.verdict = Some(verdict_str.to_owned());
    summary.reason = Some(reason.to_owned());
    if override_mode == Some("once") {
        let _ = store.set_budget_override(fid, None).await;
    }
    Ok(summary)
}

// -- analysis jobs (`scheduler._AnalysisSpec`, `run_test_gap` … `run_modernize`) ------------------------------------

/// Static per-kind wiring for analysis jobs.
type AnalysisPromptBuilder =
    fn(&Path, &Repo, &str, &[Finding], &[Finding], &Path, i64, &str) -> anyhow::Result<String>;

struct AnalysisSpec {
    kind: RepoJobKind,
    finding_type: FindingType,
    out_plural: &'static str,
    no_output_noun: &'static str,
    scope_note: &'static str,
    prompt_builder: AnalysisPromptBuilder,
}

const TEST_GAP_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::TestGap,
    finding_type: FindingType::TestGap,
    out_plural: "test_gaps",
    no_output_noun: "gaps",
    scope_note: "Full repository scan for test coverage gaps.",
    prompt_builder: playbooks::build_test_gap_prompt,
};
const DEP_UPDATE_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::DepUpdate,
    finding_type: FindingType::DepUpdate,
    out_plural: "dep_updates",
    no_output_noun: "updates",
    scope_note: "Check all package manifests for outdated dependencies.",
    prompt_builder: playbooks::build_dep_update_prompt,
};
const REFACTOR_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::Refactor,
    finding_type: FindingType::Refactor,
    out_plural: "refactorings",
    no_output_noun: "refactorings",
    scope_note: "Scan for safe, mechanical refactoring opportunities (duplication, dead code, complexity).",
    prompt_builder: playbooks::build_refactor_prompt,
};
const MODERNIZATION_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::Modernization,
    finding_type: FindingType::Modernization,
    out_plural: "modernizations",
    no_output_noun: "modernizations",
    scope_note: "Scan for SOTA-drift modernization opportunities (deprecated/unmaintained deps, language-feature gaps, format/protocol shifts, major version debt, platform EOL).",
    prompt_builder: playbooks::build_modernization_prompt,
};
const STANDARDS_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::Standards,
    finding_type: FindingType::Standards,
    out_plural: "standards",
    no_output_noun: "standards",
    scope_note: "Full repository audit against coding standards.",
    prompt_builder: playbooks::build_standards_prompt,
};

/// Shared body for the four repo-level analysis job types (`scheduler._run_analysis_job`).
#[allow(
    clippy::too_many_lines,
    reason = "one body shared by four job kinds; the per-kind differences \
              are already factored into `AnalysisJobSpec`, so what remains \
              is a single non-branching sequence"
)]
async fn run_analysis_job(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    spec: &AnalysisSpec,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
    let kind = spec.kind;

    if !rpath.exists() {
        let _ = store
            .log_event(
                "error",
                &format!("{kind} {rname}: repo not cloned"),
                None,
                None,
            )
            .await;
        anyhow::bail!("repo not cloned");
    }
    let db = &repo.default_branch;
    sync_repo(
        store,
        &repo.url,
        &rpath,
        db,
        &format!("{kind} {rname}"),
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        rid,
        JobKind::from(kind),
        cfg.hunt_cap_tokens,
        false,
        &format!("{kind} {rname}"),
        None,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };
    let job = match store
        .create_job(
            kind.into(),
            rid,
            None,
            cap,
            JobState::Running,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(kind.into(), Some(rname), None, e),
    };
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("job{job}.{}.json", spec.out_plural));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));

    let suppressions = store
        .suppressions(rid, kind.as_str())
        .await
        .unwrap_or_default();
    let known = store
        .known_active(rid, kind.as_str())
        .await
        .unwrap_or_default();
    let repo_notes = Store::repo_notes(&cfg.work_root, rid);
    let prompt = (spec.prompt_builder)(
        &cfg.root,
        repo,
        spec.scope_note,
        &suppressions,
        &known,
        &out_path,
        cfg.hunt_max_findings,
        &repo_notes,
    )?;
    let model = cfg.model_for("hunt");
    let rr = backend
        .run(&rpath, &prompt, cap, cfg.hunt_max_wall_s, JobClass::Hunt)
        .await?;
    let state = record_job(store, job, &rr, model)
        .await
        .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(kind.into()),
        repo: Some(rname.to_owned()),
        job_id: Some(job),
        state: Some(state),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };

    if out_path.exists() {
        let counts = ingest_findings(
            store,
            rid,
            &out_path,
            Some(spec.finding_type),
            Some(job),
            None,
        )
        .await;
        let _ = store
            .log_event(
                kind.as_str(),
                &format!(
                    "{rname}: job {job} {state} -- +{} new / {} dup / {} invalid ({} tok)",
                    counts.inserted, counts.duplicates, counts.invalid, rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
        if state == JobState::Done && counts.invalid == 0 {
            // Update last_{kind}_at timestamp
            let _ = store.set_last_kind_at(rid, kind).await;
        }
        summary.ingest = Some(counts);
    } else {
        let _ = store
            .log_event(
                kind.as_str(),
                &format!(
                    "{rname}: job {job} {state}, no {} file ({} tok)",
                    spec.no_output_noun, rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
    }
    Ok(summary)
}

pub async fn run_test_gap(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &TEST_GAP_SPEC, backend).await
}
pub async fn run_dep_update(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    let rpath = std::path::PathBuf::from(&repo.path);
    if !rpath.exists() {
        let _ = store
            .log_event(
                "error",
                &format!("dep_update {}: repo not cloned", repo.name),
                None,
                None,
            )
            .await;
        anyhow::bail!("repo not cloned");
    }

    // Sync to default branch first
    sync_repo(
        store,
        &repo.url,
        &rpath,
        &repo.default_branch,
        "dep_update",
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Try Renovate (zero tokens) — fall back to AI if unavailable
    let candidates = tokio::task::spawn_blocking({
        let rp = rpath.clone();
        let rn = repo.name.clone();
        move || crate::dep_scan::scan_repo(&rp, &rn, 120)
    })
    .await
    .ok()
    .flatten();

    match candidates {
        Some(ref c) if !c.is_empty() => {
            // Write candidates to JSON and ingest via the standard path
            let out_path = cfg
                .work_root
                .join("out")
                .join(format!("dep_scan_{}.json", repo.id));
            let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(std::path::Path::new(".")));
            let json_entries: Vec<_> = c
                .iter()
                .map(|cand| {
                    serde_json::json!({
                        "fingerprint": cand.fingerprint,
                        "file": cand.file,
                        "ecosystem": cand.ecosystem,
                        "package": cand.package,
                        "current_version": cand.current_version,
                        "latest_version": cand.latest_version,
                        "update_type": cand.update_type,
                        "severity": cand.severity,
                        "confidence": cand.confidence,
                        "summary": cand.summary,
                        "detail": cand.detail,
                    })
                })
                .collect();
            let _ = std::fs::write(
                &out_path,
                serde_json::to_string_pretty(&json_entries).unwrap_or_default(),
            );

            let counts = crate::ingest::ingest_findings(
                store,
                repo.id,
                &out_path,
                Some(FindingType::DepUpdate),
                None,
                None,
            )
            .await;
            let _ = store
                .log_event(
                    "dep_update",
                    &format!(
                        "{}: renovate scan +{} new / {} dup / {} invalid (0 tok)",
                        repo.name, counts.inserted, counts.duplicates, counts.invalid,
                    ),
                    None,
                    None,
                )
                .await;

            // Only advance timestamp after successful ingestion (no invalid entries)
            if counts.invalid == 0 {
                let _ = store.set_last_dep_update(repo.id).await;
            }

            Ok(CycleSummary {
                kind: Some(RepoJobKind::DepUpdate.into()),
                repo: Some(repo.name.clone()),
                state: Some(JobState::Done),
                ingest: Some(counts),
                ..Default::default()
            })
        }
        _ => {
            // Renovate not available or found nothing — fall back to AI
            tracing::info!(
                "dep_update {}: renovate unavailable or empty, falling back to AI",
                repo.name
            );
            run_analysis_job(store, cfg, repo, &DEP_UPDATE_SPEC, backend).await
        }
    }
}
pub async fn run_refactor(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &REFACTOR_SPEC, backend).await
}
pub async fn run_modernize(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &MODERNIZATION_SPEC, backend).await
}
pub async fn run_standards(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &STANDARDS_SPEC, backend).await
}

fn extract_pr_url(text: &str) -> Option<String> {
    for word in text.split_whitespace() {
        if word.starts_with("https://")
            && let Some(idx) = word.find("/pull/")
        {
            let after = &word[idx + 6..];
            let digit_end = after
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after.len());
            if digit_end > 0 {
                return Some(word[..idx + 6 + digit_end].to_owned());
            }
        }
    }
    None
}

/// Run a fix job (`scheduler.run_fix`).
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: branch, worker, tests, commit, push, PR. \
              Each step's failure path must unwind the ones before it, so \
              the ordering is the logic and splitting it hides the unwind"
)]
pub async fn run_fix(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    let fid = finding.id;
    if finding.status != FindingStatus::Queued {
        return Ok(CycleSummary {
            kind: Some(FindingJobKind::Fix.into()),
            skipped: Some(format!("finding #{fid} is {}, not queued", finding.status)),
            ..Default::default()
        });
    }
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("fix #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let rpath = PathBuf::from(&repo.path);
    let is_bug = finding.kind == FindingType::Bug;
    let is_modernization = finding.kind == FindingType::Modernization;
    // Build branch name from summary slug
    let slug: String = finding
        .summary
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = &slug[..slug.len().min(40)];
    let slug = slug.trim_end_matches('-');
    let branch_prefix = if is_bug {
        "fix"
    } else if is_modernization {
        "modernize"
    } else {
        "improve"
    };
    let branch = format!("{branch_prefix}/{slug}-{fid}");
    let worktree = cfg.work_root.join("wt").join(format!("f{fid}"));
    let _ = std::fs::create_dir_all(worktree.parent().unwrap_or(Path::new(".")));

    // Clean up old worktree if exists
    if worktree.exists() {
        let rps = rpath.to_string_lossy().to_string();
        let wts = worktree.to_string_lossy().to_string();
        let br = branch.clone();
        tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                30,
            );
            run_cmd_sync(&["git", "-C", &rps, "branch", "-D", &br], 30);
        })
        .await
        .ok();
        let _ = store
            .log_event(
                "fix",
                &format!("#{fid}: reclaimed stale worktree from prior attempt"),
                None,
                Some(fid),
            )
            .await;
    }

    // Create worktree
    let db = repo.default_branch.clone();
    let rps = rpath.to_string_lossy().to_string();
    let wts = worktree.to_string_lossy().to_string();
    let br = branch.clone();
    let db2 = db.clone();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        let add = |start_point: &str| {
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &rps,
                    "worktree",
                    "add",
                    "-b",
                    &br,
                    &wts,
                    start_point,
                ],
                30,
            )
        };
        let (rc, out) = add(&format!("origin/{db2}"));
        if rc == 0 {
            return (rc, out);
        }
        let (rc, out) = add(&db2);
        if rc == 0 {
            return (rc, out);
        }
        // A prior attempt can leave the branch behind with its worktree
        // gone: the branch is only deleted on the path where the worktree
        // directory still exists. `worktree add -b` then fails with
        // "a branch named ... already exists" on every later cycle, which
        // is a retry loop no amount of waiting resolves — observed
        // 2026-09-15, broken only by restarting the daemon. The branch
        // belongs to this finding and its work was abandoned when the
        // attempt failed, so reclaiming the name is safe: a finding whose
        // branch reached a PR is `pr_open` and never re-enters `fix`.
        run_cmd_sync(&["git", "-C", &rps, "worktree", "prune"], 30);
        run_cmd_sync(&["git", "-C", &rps, "branch", "-D", &br], 30);
        add(&format!("origin/{db2}"))
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("fix #{fid}: worktree add failed: {tail}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("worktree add failed: {tail}");
    }

    let drop_worktree = |delete_branch: bool| {
        let rps = rpath.to_string_lossy().to_string();
        let wts = worktree.to_string_lossy().to_string();
        let br = branch.clone();
        tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                30,
            );
            if delete_branch {
                run_cmd_sync(&["git", "-C", &rps, "branch", "-D", &br], 30);
            }
        })
    };

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Fix.into(),
        cfg.fix_cap_tokens,
        override_mode.is_some(),
        &format!("fix #{fid}"),
        Some(fid),
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => {
            let _ = drop_worktree(true).await;
            return Ok(*d);
        }
    };

    let job = match store
        .create_job(
            FindingJobKind::Fix.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            let _ = drop_worktree(true).await;
            return job_refused(FindingJobKind::Fix.into(), Some(&repo.name), Some(fid), e);
        }
    };

    // set_in_progress: fixing -> fallback queued
    let _ = store.set_in_progress(fid, FindingStatus::Fixing).await;

    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = match if is_bug {
        playbooks::build_fix_prompt(&cfg.root, finding, &worktree, &branch, &repo, &repo_notes)
    } else if is_modernization {
        playbooks::build_apply_modernization_prompt(
            &cfg.root,
            finding,
            &worktree,
            &branch,
            &repo,
            &repo_notes,
        )
    } else {
        playbooks::build_apply_improvement_prompt(
            &cfg.root,
            finding,
            &worktree,
            &branch,
            &repo,
            &repo_notes,
        )
    } {
        Ok(p) => p,
        Err(e) => {
            let _ = store
                .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
                .await;
            anyhow::bail!("{e}");
        }
    };
    let model = cfg.model_for("fix");
    let rr = match backend
        .run(&worktree, &prompt, cap, cfg.fix_max_wall_s, JobClass::Fix)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = store
                .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
                .await;
            anyhow::bail!("{e}");
        }
    };
    let state = record_job(store, job, &rr, model)
        .await
        .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Fix.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        branch: Some(branch.clone()),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };

    // Check for decline/blocked file
    let decline_name = if is_bug {
        "NOT-A-BUG.md"
    } else {
        "DECLINED.md"
    };
    let decline_file = worktree.join(decline_name);
    let blocked_file = worktree.join("BLOCKED.md");
    let outcome_file = if decline_file.exists() {
        Some(decline_file.clone())
    } else if blocked_file.exists() {
        Some(blocked_file.clone())
    } else {
        None
    };
    if let Some(ref ofile) = outcome_file {
        let raw = std::fs::read_to_string(ofile).unwrap_or_default();
        let reason: String = raw.chars().take(500).collect();
        let _ = store
            .set_finding_verdict(fid, FindingStatus::Rejected, &reason)
            .await;
        let _ = store.clear_fix_attempts(fid).await;
        let first_line: String = reason
            .lines()
            .next()
            .map(|l| l.chars().take(120).collect())
            .unwrap_or_default();
        let verb = if ofile == &decline_file {
            "rejected"
        } else {
            "blocked"
        };
        let _ = store
            .log_event(
                "fix",
                &format!("#{fid} {verb} by worker: {first_line}"),
                Some(job),
                Some(fid),
            )
            .await;
        let _ = drop_worktree(true).await;
        summary.outcome = Some("rejected".into());
        let _ = store
            .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
            .await;
        return Ok(summary);
    }

    // Check for commits + PR description
    let db = repo.default_branch.clone();
    let wts = worktree.to_string_lossy().to_string();
    let db2 = db.clone();
    let (rc, commits) = tokio::task::spawn_blocking(move || {
        let (rc, out) = run_cmd_sync(
            &[
                "git",
                "-C",
                &wts,
                "log",
                &format!("origin/{db2}..HEAD"),
                "--oneline",
            ],
            30,
        );
        if rc != 0 {
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &wts,
                    "log",
                    &format!("{db2}..HEAD"),
                    "--oneline",
                ],
                30,
            )
        } else {
            (rc, out)
        }
    })
    .await
    .unwrap_or((127, String::new()));
    let commits = commits.trim().to_owned();

    let pr_desc = worktree.join("PR-DESCRIPTION.md");
    let mut failure: Option<String> = None;

    if rc == 0 && !commits.is_empty() && pr_desc.exists() {
        let f = forge::forge_for(repo.forge);
        let push_url = f.ssh_url(&repo.url);
        let wts = worktree.to_string_lossy().to_string();
        let pu = push_url.clone();
        let (prc, pout) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(&["git", "-C", &wts, "push", "--force", &pu, "HEAD"], 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if prc == 0 {
            let owner_slug = f.owner_repo(&repo.url);
            if let Some((_owner, _slug_name)) = owner_slug {
                let body = std::fs::read_to_string(&pr_desc).unwrap_or_default();
                let wts = worktree.to_string_lossy().to_string();
                let (_trc, title) = tokio::task::spawn_blocking(move || {
                    run_cmd_sync(&["git", "-C", &wts, "log", "-1", "--format=%s"], 30)
                })
                .await
                .unwrap_or((127, branch.clone()));
                let title = title.trim();
                let title = if title.is_empty() { &branch } else { title };
                match f.create_pr(&rpath, &branch, &db, title, &body) {
                    Ok(pr_url) => {
                        let _ = store.set_finding_pr_open(fid, &pr_url).await;
                        let _ = store.clear_fix_attempts(fid).await;
                        let _ = store
                            .log_event(
                                "ship",
                                &format!("#{fid} draft PR: {pr_url}"),
                                Some(job),
                                Some(fid),
                            )
                            .await;
                        let _ = drop_worktree(false).await;
                        summary.outcome = Some("pr_open".into());
                        summary.pr_url = Some(pr_url);
                        if override_mode == Some("once") {
                            let _ = store.set_budget_override(fid, None).await;
                        }
                        let _ = store
                            .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
                            .await;
                        return Ok(summary);
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        if err_msg.contains("already exists")
                            && let Some(pr_url) = extract_pr_url(&err_msg)
                        {
                            let _ = store.set_finding_pr_open(fid, &pr_url).await;
                            let _ = store.clear_fix_attempts(fid).await;
                            let _ = store
                                .log_event(
                                    "ship",
                                    &format!("#{fid} recovered existing PR: {pr_url}"),
                                    Some(job),
                                    Some(fid),
                                )
                                .await;
                            let _ = drop_worktree(false).await;
                            summary.outcome = Some("pr_open".into());
                            summary.pr_url = Some(pr_url);
                            if override_mode == Some("once") {
                                let _ = store.set_budget_override(fid, None).await;
                            }
                            let _ = store
                                .finalize_in_progress(
                                    fid,
                                    FindingStatus::Fixing,
                                    FindingStatus::Queued,
                                )
                                .await;
                            return Ok(summary);
                        }
                        let head: String = err_msg.chars().take(300).collect();
                        failure = Some(format!("PR create failed: {head}"));
                    }
                }
            } else {
                failure = Some(format!("unparseable repo url for PR: {:?}", repo.url));
            }
        } else {
            let tail = crate::util::tail(&pout, 300);
            failure = Some(format!("push failed: {tail}"));
        }
    } else if failure.is_none() {
        failure = Some(if state == JobState::Done {
            if commits.is_empty() {
                "no commits".to_owned()
            } else {
                "no PR-DESCRIPTION.md".to_owned()
            }
        } else {
            format!("worker {state}")
        });
    }

    // Salvage
    let failure = failure.unwrap_or_else(|| "unknown failure".to_owned());
    let streak = store.record_fix_attempt(fid, &failure).await.unwrap_or(1);
    let tail = crate::util::tail(&rr.stdout_tail, 300);
    if streak >= MAX_CONSECUTIVE_SAME_FAILURE {
        let _ = store
            .set_finding_verdict(
                fid,
                FindingStatus::Rejected,
                &format!(
                    "stuck: {streak} consecutive fix attempts hit the same failure: {failure}"
                ),
            )
            .await;
        let _ = store.clear_fix_attempts(fid).await;
        let _ = store.log_event("fix",
            &format!("#{fid} gave up after {streak} identical failures ({failure}); worktree kept at {}. tail: {tail}", worktree.display()),
            Some(job), Some(fid),
        ).await;
        let _ = drop_worktree(true).await;
        summary.outcome = Some("stuck".into());
        summary.failure = Some(failure);
        summary.attempts = Some(streak);
    } else {
        let _ = store.set_finding_status(fid, FindingStatus::Queued).await;
        let _ = store
            .log_event(
                "fix",
                &format!(
                    "#{fid} incomplete ({failure}); worktree kept at {}. tail: {tail}",
                    worktree.display()
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("requeued".into());
        summary.failure = Some(failure);
        summary.worktree = Some(worktree.to_string_lossy().into_owned());
    }
    if override_mode == Some("once") {
        let _ = store.set_budget_override(fid, None).await;
    }
    let _ = store
        .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
        .await;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// sync_prs helpers (`scheduler._iso_ms` … `scheduler._attention_fingerprint`)
// ---------------------------------------------------------------------------

/// ISO-8601 / RFC-3339 timestamp -> epoch ms (0 when absent/unparseable).
/// GitHub emits `2024-01-15T12:30:45Z`; GitLab may use `+00:00` suffix.
/// Handles both, plus the offset-less variant (treated as UTC).
#[allow(clippy::many_single_char_names)]
fn iso_ms(ts: &str) -> i64 {
    if ts.is_empty() {
        return 0;
    }
    // Try the standard-library approach: DateTime::parse_from_rfc3339 equivalent
    // via jiff/time is unavailable, so parse manually.
    // Expected: YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM|)
    let (date_time_part, tz_offset_secs) = if let Some(pos) = ts.rfind('Z') {
        (&ts[..pos], 0i64)
    } else if let Some(pos) = ts.rfind('+').filter(|&p| p > 10) {
        // The +10 filter skips a '+' that might appear in the date portion
        let tz = &ts[pos + 1..];
        let secs = parse_tz_offset(tz);
        (&ts[..pos], secs)
    } else if let Some(pos) = ts.rfind('-').filter(|&p| p > 10) {
        let tz = &ts[pos + 1..];
        let secs = parse_tz_offset(tz);
        (&ts[..pos], -secs)
    } else {
        // No timezone suffix — treat as UTC
        (ts, 0i64)
    };
    // Parse YYYY-MM-DDTHH:MM:SS[.frac]
    let parts: Vec<&str> = date_time_part.splitn(2, 'T').collect();
    if parts.len() != 2 {
        return 0;
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    let time_full = parts[1];
    let time_parts: Vec<&str> = time_full.splitn(2, '.').collect();
    let hms: Vec<&str> = time_parts[0].split(':').collect();
    if date_parts.len() != 3 || hms.len() != 3 {
        return 0;
    }
    let (Ok(y), Ok(mo), Ok(d)) = (
        date_parts[0].parse::<i64>(),
        date_parts[1].parse::<u32>(),
        date_parts[2].parse::<u32>(),
    ) else {
        return 0;
    };
    let (Ok(h), Ok(mi), Ok(s)) = (
        hms[0].parse::<i64>(),
        hms[1].parse::<i64>(),
        hms[2].parse::<i64>(),
    ) else {
        return 0;
    };
    // Fractional seconds -> ms
    let frac_ms: i64 = if time_parts.len() == 2 {
        let frac = time_parts[1];
        // Timestamps come from the forge API; a malformed non-ASCII
        // fraction must not panic the slice.
        let digits = frac.len().min(3);
        let n: i64 = frac.get(..digits).unwrap_or("").parse().unwrap_or(0);
        // Scale to ms: if 1 digit, *100; 2 digits, *10; 3 digits, *1
        n * 10i64.pow((3 - digits) as u32)
    } else {
        0
    };
    // days since epoch using a simplified civil_to_days (Hinnant's algorithm)
    let epoch_days = civil_to_days(y, mo, d);
    let epoch_secs = epoch_days * 86400 + h * 3600 + mi * 60 + s - tz_offset_secs;
    epoch_secs * 1000 + frac_ms
}

/// Parse HH:MM timezone offset -> total seconds.
fn parse_tz_offset(tz: &str) -> i64 {
    let parts: Vec<&str> = tz.split(':').collect();
    let h: i64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let m: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    h * 3600 + m * 60
}

/// Proleptic-Gregorian days since 1970-01-01 (Hinnant's `civil_from_days` inverse).
fn civil_to_days(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + i64::from(doe) - 719_468
}

/// Max of createdAt from comments + submittedAt from reviews -> epoch ms.
fn latest_activity_ms(pr: &PrView) -> i64 {
    let comment_stamps = pr.comments.iter().map(|c| iso_ms(&c.created_at));
    let review_stamps = pr.reviews.iter().map(|r| iso_ms(&r.submitted_at));
    comment_stamps.chain(review_stamps).max().unwrap_or(0)
}

/// (human summary, `any_failing`, sorted-unique failing check names).
fn checks_summary(rollup: &[GhCheckRun]) -> (Option<String>, bool, Vec<String>) {
    if rollup.is_empty() {
        return (None, false, Vec::new());
    }
    let named: Vec<(&str, CheckConclusion)> = rollup
        .iter()
        .map(|c| {
            let name = c.name.as_deref().or(c.context.as_deref()).unwrap_or("?");
            let raw = c.conclusion.as_deref().or(c.state.as_deref()).unwrap_or("");
            (name, CheckConclusion::parse(raw))
        })
        .collect();

    let mut failing_set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut fail_count = 0usize;
    let mut pass_count = 0usize;
    for &(name, concl) in &named {
        if concl.is_failing() {
            failing_set.insert(name);
            fail_count += 1;
        } else if concl.is_passing() {
            pass_count += 1;
        }
    }
    let pending = named.len() - fail_count - pass_count;
    let failing_names: Vec<String> = failing_set.iter().map(|s| (*s).to_owned()).collect();

    let mut parts = vec![format!("{pass_count} pass")];
    if !failing_names.is_empty() {
        parts.push(format!("{fail_count} fail"));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    (
        Some(parts.join(" / ")),
        !failing_names.is_empty(),
        failing_names,
    )
}

/// Fingerprint of static (non-comment) attention reasons.
/// `review:CHANGES_REQUESTED` | mergeable:CONFLICTING | checks:name1,name2
/// None when nothing static is wrong.
fn attention_fingerprint(pr: &PrView, failing_names: &[String]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if pr.review_decision == ReviewDecision::ChangesRequested {
        parts.push("review:CHANGES_REQUESTED".to_owned());
    }
    if pr.mergeable == Mergeable::Conflicting {
        parts.push("mergeable:CONFLICTING".to_owned());
    }
    if !failing_names.is_empty() {
        parts.push(format!("checks:{}", failing_names.join(",")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("|"))
    }
}

/// Sync PRs for all `pr_open` findings (`scheduler.sync_prs`).
#[allow(
    clippy::too_many_lines,
    reason = "one pass per open PR with a flat decision table over PR \
              state (merged, closed, review comments, checks); the arms \
              read as a table only while they sit together"
)]
pub async fn sync_prs(store: &Store, _cfg: &Config) -> SyncResult {
    let mut summary = SyncResult::default();
    let findings = store
        .list_findings(&FindingFilter {
            status: Some(FindingStatus::PrOpen),
            ..FindingFilter::default()
        })
        .await
        .unwrap_or_default();

    for f in &findings {
        let fid = f.id;
        let url = match f.pr_url.as_deref() {
            Some(u) if !u.is_empty() => u,
            _ => {
                // pr_open with no PR URL = a fix attempt failed before creating
                // the PR. Requeue so it gets retried instead of sitting stuck.
                tracing::warn!(
                    finding_id = fid,
                    "pr_open finding has no pr_url — requeueing"
                );
                let _ = store.set_finding_status(fid, FindingStatus::Queued).await;
                let _ = store.log_event(
                    "fix", &format!("#{fid} requeued: pr_open with no pr_url (prior fix failed before PR creation)"),
                    None, Some(fid),
                ).await;
                continue;
            }
        };
        let Ok(Some(repo)) = store.get_repo_by_id(f.repo_id).await else {
            let _ = store
                .log_event(
                    "error",
                    &format!("sync #{fid}: repo {} missing", f.repo_id),
                    None,
                    Some(fid),
                )
                .await;
            summary.errors += 1;
            continue;
        };
        let fg = forge::forge_for(repo.forge);
        let Some((_slug, pr_number)) = fg.parse_pr_url(url) else {
            let _ = store
                .log_event(
                    "error",
                    &format!("sync #{fid}: unparseable pr_url {url:?}"),
                    None,
                    Some(fid),
                )
                .await;
            summary.errors += 1;
            continue;
        };
        let pr = match fg.view_pr_sync(&repo.url, pr_number) {
            Ok(pr) => pr,
            Err(e) => {
                let _ = store
                    .log_event(
                        "error",
                        &format!("sync #{fid}: PR view failed: {e}"),
                        None,
                        Some(fid),
                    )
                    .await;
                summary.errors += 1;
                continue;
            }
        };

        match pr.state {
            forge::PrState::Merged => {
                let _ = store.set_finding_status(fid, FindingStatus::Merged).await;
                let _ = store.mark_pr_merged(fid, pr_number, now_ms()).await;
                let _ = store
                    .log_event("ship", &format!("#{fid} PR merged: {url}"), None, Some(fid))
                    .await;
                summary.merged += 1;
                continue;
            }
            forge::PrState::Closed => {
                let _ = store.set_finding_verdict(fid, FindingStatus::Rejected,
                    "PR closed without merge -- treat this bug class/location as human-rejected",
                ).await;
                let _ = store.mark_pr_closed(fid, pr_number, now_ms()).await;
                let _ = store
                    .log_event(
                        "verdict",
                        &format!("#{fid} PR closed without merge: {url}"),
                        None,
                        Some(fid),
                    )
                    .await;
                summary.closed += 1;
                continue;
            }
            forge::PrState::Open => { /* fall through to attention logic below */ }
        }

        // -- Open PR: full attention-flagging logic (open-PR branch of `scheduler.sync_prs`) --

        let prev = store.get_pr_state(fid).await.ok().flatten();
        let last_activity = latest_activity_ms(&pr);
        let (checks, failing, failing_names) = checks_summary(&pr.status_check_rollup);

        // Baseline the engaged watermark on first sync so we don't
        // flag our own PR-creation chatter.
        let engaged: i64 = if let Some(e) = prev.as_ref().and_then(|p| p.last_engaged_activity_at) {
            e
        } else {
            let updated_ms = iso_ms(&pr.updated_at);
            std::cmp::max(updated_ms, last_activity)
        };

        // Suppression: don't re-flag a static reason identical to the
        // one an engage cycle already declined (same fingerprint AND
        // same head_sha — a push resets suppression even if the static
        // snapshot looks identical).
        let fp = attention_fingerprint(&pr, &failing_names);
        let addressed_fp = prev
            .as_ref()
            .and_then(|p| p.addressed_fingerprint.as_deref());
        let addressed_sha = prev.as_ref().and_then(|p| p.addressed_head_sha.as_deref());
        let head_sha = &pr.head_sha;
        let suppressed = fp.is_some()
            && fp.as_deref() == addressed_fp
            && addressed_sha.is_some()
            && Some(head_sha.as_str()) == addressed_sha;

        let mut reasons: Vec<&str> = Vec::new();
        if last_activity > engaged {
            reasons.push("new_comments");
        }
        if !suppressed {
            if pr.review_decision == ReviewDecision::ChangesRequested {
                reasons.push("changes_requested");
            }
            if pr.mergeable == Mergeable::Conflicting {
                reasons.push("conflict");
            }
            if failing {
                reasons.push("checks_failing");
            }
        }
        let attention: Option<String> = if reasons.is_empty() {
            None
        } else {
            Some(reasons.join(","))
        };

        // attention_since fairness: only touch when the reason changes.
        let prev_attention = prev.as_ref().and_then(|p| p.needs_attention.as_deref());
        let mut attention_since_val: Option<Option<i64>> = None;
        if attention.as_deref() != prev_attention {
            attention_since_val = if attention.is_some() {
                Some(Some(now_ms()))
            } else {
                Some(None)
            };
        }
        let mut addr_fp: Option<Option<String>> = None;
        if addressed_fp.is_some()
            && (fp.as_deref() != addressed_fp || Some(head_sha.as_str()) != addressed_sha)
        {
            addr_fp = Some(None);
        }
        let clear_addressed = addr_fp.is_some();
        let data = SyncPrData {
            pr_number,
            state: pr.state.as_str().into(),
            mergeable: pr.mergeable.as_str().into(),
            checks,
            head_ref: pr.head_ref.clone(),
            head_sha: head_sha.clone(),
            last_activity_at: last_activity,
            last_engaged_activity_at: engaged,
            needs_attention: attention.clone(),
            attention_fingerprint: fp,
            synced_at: now_ms(),
            attention_since: attention_since_val,
            clear_addressed,
        };
        let _ = store.sync_pr_open(fid, &data).await;

        if attention.is_some() && attention.as_deref() != prev_attention {
            let _ = store
                .log_event(
                    "engage",
                    &format!(
                        "#{fid} PR #{pr_number} needs attention: {}",
                        attention.as_deref().unwrap_or("")
                    ),
                    None,
                    Some(fid),
                )
                .await;
        }
        summary.synced += 1;
        if attention.is_some() {
            summary.attention += 1;
        }
    }
    summary
}

/// Run engage job (`scheduler.run_engage`).
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: check out the PR branch, run the worker, \
              test, push, reply. Shares the fix pipeline's unwind structure"
)]
pub async fn run_engage(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    let fid = finding.id;
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let Ok(Some(ps)) = store.get_pr_state(fid).await else {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: no pr_state/head_ref -- sync first"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("no pr_state");
    };
    let head_ref = match &ps.head_ref {
        Some(hr) if !hr.is_empty() => hr.clone(),
        _ => {
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: no head_ref"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("no pr_state");
        }
    };
    let pr_number = if let Some(n) = ps.pr_number {
        n
    } else {
        let fg = forge::forge_for(repo.forge);
        if let Some((_slug, num)) = finding.pr_url.as_deref().and_then(|u| fg.parse_pr_url(u)) {
            tracing::warn!(
                finding_id = fid,
                pr_number = num,
                "self-healed missing pr_number from pr_url"
            );
            let _ = store.set_pr_number(fid, num).await;
            num
        } else {
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: no pr_number and no parseable pr_url"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("no pr_number");
        }
    };
    if head_ref == repo.default_branch {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: refusing -- head_ref equals default branch {head_ref:?}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("head_ref equals default branch");
    }

    let rpath = PathBuf::from(&repo.path);
    let worktree = cfg.work_root.join("wt").join(format!("e{fid}"));
    let _ = std::fs::create_dir_all(worktree.parent().unwrap_or(Path::new(".")));
    if worktree.exists() {
        let rps = rpath.to_string_lossy().to_string();
        let wts = worktree.to_string_lossy().to_string();
        let _ = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                30,
            );
        })
        .await;
        let _ = store
            .log_event(
                "engage",
                &format!("#{fid}: reclaimed stale worktree from prior attempt"),
                None,
                Some(fid),
            )
            .await;
    }

    // Fetch PR branch, create worktree
    let rps = rpath.to_string_lossy().to_string();
    let hr = head_ref.clone();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(&["git", "-C", &rps, "fetch", "origin", &hr], 600)
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: fetch {head_ref} failed: {tail}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("fetch failed: {tail}");
    }
    let rps = rpath.to_string_lossy().to_string();
    let wts = worktree.to_string_lossy().to_string();
    let hr = head_ref.clone();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &[
                "git",
                "-C",
                &rps,
                "worktree",
                "add",
                "--detach",
                &wts,
                &format!("origin/{hr}"),
            ],
            30,
        )
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: worktree add failed: {tail}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("worktree add failed: {tail}");
    }
    // Checkout branch
    let wts = worktree.to_string_lossy().to_string();
    let hr = head_ref.clone();
    let _ = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &[
                "git",
                "-C",
                &wts,
                "checkout",
                "-B",
                &hr,
                &format!("origin/{hr}"),
            ],
            30,
        )
    })
    .await;

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Engage.into(),
        cfg.fix_cap_tokens,
        override_mode.is_some(),
        &format!("engage #{fid}"),
        Some(fid),
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => {
            let rps = rpath.to_string_lossy().to_string();
            let wts = worktree.to_string_lossy().to_string();
            let _ = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                    30,
                );
            })
            .await;
            return Ok(*d);
        }
    };

    let fg = forge::forge_for(repo.forge);
    let pr = match fg.view_pr_engage(&repo.url, pr_number) {
        Ok(p) => p,
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: PR/MR view failed: {e}"),
                    None,
                    Some(fid),
                )
                .await;
            let rps = rpath.to_string_lossy().to_string();
            let wts = worktree.to_string_lossy().to_string();
            let _ = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                    30,
                );
            })
            .await;
            anyhow::bail!("PR/MR view failed");
        }
    };

    let job = match store
        .create_job(
            FindingJobKind::Engage.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            let rps = rpath.to_string_lossy().to_string();
            let wts = worktree.to_string_lossy().to_string();
            let _ = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                    30,
                );
            })
            .await;
            return job_refused(
                FindingJobKind::Engage.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };

    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = playbooks::build_engage_prompt(
        &cfg.root,
        finding,
        &worktree,
        &head_ref,
        &repo,
        &pr,
        &ps,
        &repo_notes,
    )?;
    let model = cfg.model_for("fix");
    let rr = backend
        .run(&worktree, &prompt, cap, cfg.fix_max_wall_s, JobClass::Fix)
        .await?;
    let state = record_job(store, job, &rr, model)
        .await
        .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Engage.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        pr_number: Some(pr_number),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };

    // Check for WITHDRAW.md
    let withdraw = worktree.join("WITHDRAW.md");
    if withdraw.exists() {
        let reason = std::fs::read_to_string(&withdraw).unwrap_or_default();
        if fg.owner_repo(&repo.url).is_some() {
            let comment: String = reason.chars().take(800).collect();
            if let Err(err) = fg.close_pr(&repo.url, pr_number, &comment) {
                // close_pr posts the withdrawal reason and only then closes,
                // so a failure here means the PR is still OPEN on the forge.
                // Recording the verdict anyway would mark it closed locally
                // and set the finding Rejected -- and sync_prs only revisits
                // pr_open findings, so nothing would ever reconcile it.
                //
                // Leaving the finding untouched is not enough either: it
                // stays in list_attention, which pick_next ranks second, so
                // a persistently failing forge would monopolise every cycle
                // running a fresh worker each time. Unlike fix/recheck/
                // harvest there is no engage attempt counter to cap that.
                // So mark THIS attention reason addressed, exactly as a
                // reply-only engage does: the finding stops being re-picked
                // until the PR sees genuinely new activity (the attention
                // fingerprint or head_sha changes), and the error event
                // below is what surfaces it in the meantime.
                tracing::warn!(finding = fid, pr = pr_number, error = %err, "close_pr failed");
                let _ = store
                    .mark_pr_engaged(
                        fid,
                        ps.last_activity_at.unwrap_or_else(now_ms),
                        now_ms(),
                        ps.attention_fingerprint.as_deref(),
                        ps.head_sha.as_deref(),
                    )
                    .await;
                let _ = store
                    .log_event(
                        "error",
                        &format!(
                            "#{fid} withdrawal aborted: PR #{pr_number} could not be closed \
                             (retries when the PR next changes)"
                        ),
                        Some(job),
                        Some(fid),
                    )
                    .await;
                let rps = rpath.to_string_lossy().to_string();
                let wts = worktree.to_string_lossy().to_string();
                let _ = tokio::task::spawn_blocking(move || {
                    run_cmd_sync(
                        &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                        30,
                    );
                })
                .await;
                summary.outcome = Some("withdraw-failed".into());
                return Ok(summary);
            }
        }
        let reason_short: String = reason.chars().take(500).collect();
        let _ = store
            .set_finding_verdict(fid, FindingStatus::Rejected, &reason_short)
            .await;
        let _ = store.mark_pr_closed(fid, pr_number, now_ms()).await;
        let first_line: String = reason
            .lines()
            .next()
            .map(|l| l.chars().take(120).collect())
            .unwrap_or_default();
        let _ = store
            .log_event(
                "verdict",
                &format!("#{fid} withdrawn by engage worker: {first_line}"),
                Some(job),
                Some(fid),
            )
            .await;
        summary.ingest = ingest_followups(store, repo.id, &worktree, fid, job, "engage").await;
        let rps = rpath.to_string_lossy().to_string();
        let wts = worktree.to_string_lossy().to_string();
        let _ = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                30,
            );
        })
        .await;
        summary.outcome = Some("withdrawn".into());
        return Ok(summary);
    }

    // Push + reply
    let mut failure: Option<String> = if state == JobState::Done {
        None
    } else {
        Some(format!("worker {state}"))
    };
    let mut pushed = false;
    let mut replied = false;
    if failure.is_none() {
        let wts = worktree.to_string_lossy().to_string();
        let hr = head_ref.clone();
        let (rc, commits) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &wts,
                    "log",
                    &format!("origin/{hr}..HEAD"),
                    "--oneline",
                ],
                30,
            )
        })
        .await
        .unwrap_or((127, String::new()));
        if rc == 0 && !commits.trim().is_empty() {
            let push_url = fg.ssh_url(&repo.url);
            let wts = worktree.to_string_lossy().to_string();
            let hr = head_ref.clone();
            let (prc, pout) = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &[
                        "git",
                        "-C",
                        &wts,
                        "push",
                        "--force",
                        &push_url,
                        &format!("HEAD:{hr}"),
                    ],
                    600,
                )
            })
            .await
            .unwrap_or((127, "spawn error".to_owned()));
            if prc == 0 {
                pushed = true;
            } else {
                let tail = crate::util::tail(&pout, 300);
                failure = Some(format!("push failed: {tail}"));
            }
        }
    }
    if failure.is_none() {
        let reply = worktree.join("PR-REPLY.md");
        if reply.exists() {
            let body = std::fs::read_to_string(&reply).unwrap_or_default();
            match fg.comment_pr(&repo.url, pr_number, &body) {
                Ok(()) => {
                    replied = true;
                }
                Err(e) => {
                    failure = Some(format!(
                        "PR comment failed: {}",
                        crate::util::tail(&e.to_string(), 300)
                    ));
                }
            }
        }
    }

    if let Some(ref fail) = failure {
        if state == JobState::Done {
            let _ = store.fail_job(job, fail.as_str()).await;
        }
        let tail = crate::util::tail(&rr.stdout_tail, 300);
        let _ = store
            .log_event(
                "engage",
                &format!(
                    "#{fid} incomplete ({fail}); worktree kept at {}. tail: {tail}",
                    worktree.display()
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("retry".into());
        summary.failure = Some(fail.clone());
        if override_mode == Some("once") {
            let _ = store.set_budget_override(fid, None).await;
        }
        return Ok(summary);
    }

    // Success: update engagement state
    let engaged_mark = if replied {
        now_ms() + 3_000
    } else {
        ps.last_activity_at.unwrap_or_else(now_ms)
    };
    let addressed_fp: Option<&str> = if pushed {
        None
    } else {
        ps.attention_fingerprint.as_deref()
    };
    let addressed_sha: Option<&str> = if pushed { None } else { ps.head_sha.as_deref() };
    let _ = store
        .mark_pr_engaged(fid, engaged_mark, now_ms(), addressed_fp, addressed_sha)
        .await;
    let did: Vec<&str> = [("pushed", pushed), ("replied", replied)]
        .iter()
        .filter(|(_, on)| *on)
        .map(|(b, _)| *b)
        .collect();
    let did_str = if did.is_empty() {
        "no-op".to_owned()
    } else {
        did.join(", ")
    };
    let _ = store
        .log_event(
            "engage",
            &format!("#{fid} PR #{pr_number} engaged ({did_str})"),
            Some(job),
            Some(fid),
        )
        .await;
    let rps = rpath.to_string_lossy().to_string();
    let wts = worktree.to_string_lossy().to_string();
    let _ = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
            30,
        );
    })
    .await;
    summary.outcome = Some("engaged".into());
    if override_mode == Some("once") {
        let _ = store.set_budget_override(fid, None).await;
    }
    Ok(summary)
}

/// Run harvest job (`scheduler.run_harvest`).
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: sync, gate budget, run the worker, \
              ingest follow-up findings"
)]
pub async fn run_harvest(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
) -> anyhow::Result<CycleSummary> {
    let fid = finding.id;
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("harvest #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let Ok(Some(ps)) = store.get_pr_state(fid).await else {
        let _ = store
            .log_event(
                "error",
                &format!("harvest #{fid}: no pr_state/pr_number -- sync first"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("no pr_state");
    };
    let pr_number = if let Some(n) = ps.pr_number {
        n
    } else {
        // Self-heal: pr_number missing in pr_state but pr_url exists on the finding.
        let fg = forge::forge_for(repo.forge);
        if let Some((_slug, num)) = finding.pr_url.as_deref().and_then(|u| fg.parse_pr_url(u)) {
            tracing::warn!(
                finding_id = fid,
                pr_number = num,
                "self-healed missing pr_number from pr_url"
            );
            let _ = store.set_pr_number(fid, num).await;
            num
        } else {
            let _ = store
                .log_event(
                    "error",
                    &format!("harvest #{fid}: no pr_number and no parseable pr_url"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("no pr_number");
        }
    };

    let rpath = PathBuf::from(&repo.path);
    let worktree = cfg.work_root.join("wt").join(format!("h{fid}"));
    let _ = std::fs::create_dir_all(worktree.parent().unwrap_or(Path::new(".")));
    if worktree.exists() {
        let rps = rpath.to_string_lossy().to_string();
        let wts = worktree.to_string_lossy().to_string();
        let _ = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                30,
            );
        })
        .await;
        let _ = store
            .log_event(
                "harvest",
                &format!("#{fid}: reclaimed stale worktree from prior attempt"),
                None,
                Some(fid),
            )
            .await;
    }

    let db = repo.default_branch.clone();
    let rps = rpath.to_string_lossy().to_string();
    let db2 = db.clone();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(&["git", "-C", &rps, "fetch", "origin", &db2], 600)
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("harvest #{fid}: fetch {db} failed: {tail}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("fetch failed: {tail}");
    }
    let rps = rpath.to_string_lossy().to_string();
    let wts = worktree.to_string_lossy().to_string();
    let db2 = db.clone();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &[
                "git",
                "-C",
                &rps,
                "worktree",
                "add",
                "--detach",
                &wts,
                &format!("origin/{db2}"),
            ],
            30,
        )
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("harvest #{fid}: worktree add failed: {tail}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("worktree add failed: {tail}");
    }

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Harvest.into(),
        cfg.fix_cap_tokens,
        override_mode.is_some(),
        &format!("harvest #{fid}"),
        Some(fid),
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => {
            let rps = rpath.to_string_lossy().to_string();
            let wts = worktree.to_string_lossy().to_string();
            let _ = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                    30,
                );
            })
            .await;
            return Ok(*d);
        }
    };

    let fg = forge::forge_for(repo.forge);
    let pr = match fg.view_pr_engage(&repo.url, pr_number) {
        Ok(p) => p,
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!("harvest #{fid}: PR/MR view failed: {e}"),
                    None,
                    Some(fid),
                )
                .await;
            let rps = rpath.to_string_lossy().to_string();
            let wts = worktree.to_string_lossy().to_string();
            let _ = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                    30,
                );
            })
            .await;
            anyhow::bail!("PR/MR view failed");
        }
    };

    let job = match store
        .create_job(
            FindingJobKind::Harvest.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            let rps = rpath.to_string_lossy().to_string();
            let wts = worktree.to_string_lossy().to_string();
            let _ = tokio::task::spawn_blocking(move || {
                run_cmd_sync(
                    &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
                    30,
                );
            })
            .await;
            return job_refused(
                FindingJobKind::Harvest.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };
    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = playbooks::build_harvest_prompt(
        &cfg.root,
        finding,
        &worktree,
        &repo.default_branch,
        &repo,
        &pr,
        pr_number,
        &repo_notes,
    )?;
    let model = cfg.model_for("fix");
    let rr = backend
        .run(&worktree, &prompt, cap, cfg.fix_max_wall_s, JobClass::Fix)
        .await?;
    let state = record_job(store, job, &rr, model)
        .await
        .unwrap_or(JobState::Failed);

    // Ingest follow-ups before dropping worktree
    let followups = ingest_followups(store, repo.id, &worktree, fid, job, "harvest").await;
    let rps = rpath.to_string_lossy().to_string();
    let wts = worktree.to_string_lossy().to_string();
    let _ = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &["git", "-C", &rps, "worktree", "remove", "--force", &wts],
            30,
        );
    })
    .await;

    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Harvest.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        pr_number: Some(pr_number),
        ingest: followups,
        ..Default::default()
    };
    if state != JobState::Done {
        let failure = format!("worker {state}");
        let streak = store
            .record_harvest_attempt(fid, &failure)
            .await
            .unwrap_or(1);
        if streak >= MAX_CONSECUTIVE_SAME_FAILURE {
            let _ = store.mark_pr_harvested(fid, now_ms()).await;
            let _ = store.clear_harvest_attempts(fid).await;
            let _ = store.log_event("error",
                &format!("harvest #{fid}: gave up after {streak} identical failures ({failure}) -- not reviewed, will not retry"),
                Some(job), Some(fid),
            ).await;
            summary.outcome = Some("stuck".into());
        } else {
            let _ = store
                .log_event(
                    "error",
                    &format!("harvest #{fid}: worker {state}, will retry"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("retry".into());
        }
        if override_mode == Some("once") {
            let _ = store.set_budget_override(fid, None).await;
        }
        return Ok(summary);
    }

    let _ = store.mark_pr_harvested(fid, now_ms()).await;
    let _ = store.clear_harvest_attempts(fid).await;
    let _ = store
        .log_event(
            "harvest",
            &format!("#{fid} PR #{pr_number} reviewed for follow-ups"),
            Some(job),
            Some(fid),
        )
        .await;
    summary.outcome = Some("harvested".into());
    if override_mode == Some("once") {
        let _ = store.set_budget_override(fid, None).await;
    }
    Ok(summary)
}

/// Run one cycle: sync PRs, `pick_next`, dispatch to the appropriate runner (`scheduler.run_cycle`).
pub async fn run_cycle(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> CycleSummary {
    // Catch-all
    match run_cycle_inner(store, cfg, backend, force_repo).await {
        Ok(result) => result,
        Err(e) => {
            let _ = store
                .log_event("error", &format!("cycle crashed: {e:?}"), None, None)
                .await;
            CycleSummary {
                error: Some(e.to_string()),
                ..Default::default()
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the scheduler's priority ladder: each tier is an ordered \
              `if` that must be read against the tiers above and below it, \
              which is exactly what extracting them into helpers destroys"
)]
async fn run_cycle_inner(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> anyhow::Result<CycleSummary> {
    // (0) Cheap PR sync
    let has_pr_open = !store
        .list_findings(&FindingFilter {
            status: Some(FindingStatus::PrOpen),
            ..FindingFilter::default()
        })
        .await?
        .is_empty();
    let sync = if has_pr_open {
        Some(sync_prs(store, cfg).await)
    } else {
        None
    };

    let picked = pick_next(store, cfg, force_repo).await?;
    if picked.is_none() {
        let result = CycleSummary {
            kind: None,
            idle: Some("no queued findings, no enabled repos".into()),
            sync,
            ..Default::default()
        };
        let _ = store
            .log_event("cycle", "idle: nothing to do", None, None)
            .await;
        return Ok(result);
    }

    let Some(candidate) = picked else {
        return Ok(CycleSummary {
            kind: None,
            idle: Some("no candidate".into()),
            ..Default::default()
        });
    };

    // Exhaustive dispatch on Candidate variant and sub-kind.
    let mut result = match &candidate {
        Candidate::Finding {
            kind, finding_id, ..
        } => {
            let finding = store
                .get_finding(*finding_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("finding {finding_id} not found"))?;
            match kind {
                FindingJobKind::Engage => run_engage(store, cfg, &finding, backend).await?,
                FindingJobKind::Harvest => run_harvest(store, cfg, &finding, backend).await?,
                FindingJobKind::Recheck => run_recheck(store, cfg, &finding, backend).await?,
                FindingJobKind::Fix => run_fix(store, cfg, &finding, backend).await?,
            }
        }
        Candidate::Repo { kind, repo_id, .. } => {
            let repo = store
                .get_repo_by_id(*repo_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))?;
            match kind {
                RepoJobKind::Hunt => run_hunt(store, cfg, &repo, backend).await?,
                RepoJobKind::TestGap => run_test_gap(store, cfg, &repo, backend).await?,
                RepoJobKind::DepUpdate => run_dep_update(store, cfg, &repo, backend).await?,
                RepoJobKind::Refactor => run_refactor(store, cfg, &repo, backend).await?,
                RepoJobKind::Modernization => run_modernize(store, cfg, &repo, backend).await?,
                RepoJobKind::Standards => run_standards(store, cfg, &repo, backend).await?,
            }
        }
    };

    // Starvation prevention for rotation kinds: bump the timestamp on
    // failed/killed/done-without-output/done-with-all-invalid so this
    // kind doesn't monopolise the rotation forever. This is SEPARATE from
    // run_analysis_job's success-gated timestamp (which governs this kind's
    // own retry cadence) — this block ensures OTHER kinds get a turn.
    if let Candidate::Repo { kind, repo_id, .. } = &candidate
        && kind.is_analysis()
    {
        let failed = matches!(result.state, Some(JobState::Killed | JobState::Failed))
            || result.error.is_some()
            || (result.state == Some(JobState::Done) && result.ingest.is_none())
            || result
                .ingest
                .as_ref()
                .is_some_and(|i| i.inserted == 0 && i.invalid > 0);
        if failed {
            let _ = store.set_last_kind_at(*repo_id, *kind).await;
        }
    }

    if sync.is_some() {
        result.sync = sync;
    }
    // Build log line
    let mut parts: Vec<String> = Vec::new();
    if let Some(k) = result.kind {
        parts.push(format!("kind={k}"));
    }
    if let Some(v) = &result.repo {
        parts.push(format!("repo={v}"));
    }
    if let Some(v) = result.finding_id {
        parts.push(format!("finding={v}"));
    }
    if let Some(v) = result.job_id {
        parts.push(format!("job={v}"));
    }
    if let Some(v) = result.state {
        parts.push(format!("state={v}"));
    }
    if let Some(v) = &result.outcome {
        parts.push(format!("outcome={v}"));
    }
    if let Some(v) = &result.skipped {
        parts.push(format!("skipped={v}"));
    }
    if let Some(v) = &result.denied {
        parts.push(format!("denied={v}"));
    }
    if let Some(v) = &result.error {
        parts.push(format!("error={v}"));
    }
    let mut msg = parts.join(", ");
    if let Some(s) = &result.sync {
        let sync_str = format!(
            "prsync {}s/{}m/{}c/{}a/{}e",
            s.synced, s.merged, s.closed, s.attention, s.errors
        );
        msg = if msg.is_empty() {
            sync_str
        } else {
            format!("{msg}; {sync_str}")
        };
    }
    let _ = store.log_event("cycle", &msg, None, None).await;
    Ok(result)
}
