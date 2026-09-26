//! Scheduler — job selection and the executors that carry it out.
//!
//! The selection half (`pick_next`, `anticipated_tokens`) is also what
//! `GET /api/summary` uses for its next-candidate preview, so the
//! endpoint and the loop cannot disagree about what runs next. The
//! `run_*` executors below are the other half.

use std::path::Path;

use crate::config::Config;
use crate::domain::{FindingJobKind, FindingStatus, FindingType, JobKind, JobState, RepoJobKind};
use crate::store::{CreatedJob, FindingFilter, Store, StoreWriteError, SyncPrData};
use crate::types::{Finding, Job, Repo};
use crate::util::now_ms;

/// Everything a resumed attempt needs and a cold one does not.
///
/// A resume is not a seventh job kind: it runs through the executor of
/// whatever kind was suspended, so it ingests, advances watermarks and
/// handles PRs exactly as that kind always does. What differs is all in
/// here — which budget the gate reserves, which row the new job points
/// back at, which transcript omp is handed, which output path the
/// continuing worker is still writing to, and the chain's workspace and
/// pinned commit, reused untouched.
#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub kind: JobKind,
    pub repo_id: i64,
    /// Repo name, carried because the events this plan's outcome logs
    /// fire from `record_job`, which never reads a repo row.
    pub repo: String,
    pub finding_id: Option<i64>,
    /// The suspended attempt being continued.
    pub predecessor_id: i64,
    /// The first attempt in the chain. Its id is the one the playbook
    /// baked into the output path, and a continuing worker is still
    /// writing there (`Store::resume_origin_job`).
    pub origin_job_id: i64,
    /// The predecessor's transcript, handed to omp verbatim. Lives in
    /// `workspace.session`.
    pub session_file: PathBuf,
    /// The chain's workspace. A resume runs in it as it is: nothing is
    /// fetched, checked out or created.
    pub workspace: crate::workspace::Workspace,
    /// The commit the chain's tree was created at (`jobs.pinned_sha`).
    pub pinned_sha: String,
    /// The reservation from [`resume_reservation`], standing in for the
    /// per-kind estimate at the budget gate.
    pub anticipated: i64,
    /// The three terms `anticipated` was computed from, kept so the
    /// event logged at dispatch can show its arithmetic. An operator
    /// looking at a large reservation needs to see whether it is a large
    /// context or a chain that has overrun, and re-deriving them at the
    /// log site would mean re-reading the transcript and the chain.
    pub ctx: i64,
    pub typical: i64,
    pub chain_spent: i64,
}

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
    /// Continue a suspended attempt instead of redoing it.
    ///
    /// `label` and `budget_override` are the tier's own: a finding tier
    /// that continues its finding's suspended attempt still shows that
    /// finding and still runs under that finding's override, exactly as
    /// the fresh candidate it replaces would have. The repo-level resume
    /// tier carries the repo name and no override.
    Resume {
        label: Option<String>,
        budget_override: Option<String>,
        /// Boxed: a plan carries its workspace's paths and dwarfs the
        /// other variants.
        plan: Box<ResumePlan>,
    },
}

impl Candidate {
    pub fn job_kind(&self) -> JobKind {
        match self {
            Self::Repo { kind, .. } => (*kind).into(),
            Self::Finding { kind, .. } => (*kind).into(),
            Self::Resume { plan, .. } => plan.kind,
        }
    }

    pub fn repo_id(&self) -> i64 {
        match self {
            Self::Repo { repo_id, .. } | Self::Finding { repo_id, .. } => *repo_id,
            Self::Resume { plan, .. } => plan.repo_id,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Repo { label, .. } | Self::Finding { label, .. } | Self::Resume { label, .. } => {
                label.as_deref()
            }
        }
    }

    /// The primary target ID: `finding_id` for finding kinds, `repo_id` for repo kinds.
    pub fn target_id(&self) -> i64 {
        match self {
            Self::Repo { repo_id, .. } => *repo_id,
            Self::Finding { finding_id, .. } => *finding_id,
            Self::Resume { plan, .. } => plan.finding_id.unwrap_or(plan.repo_id),
        }
    }

    pub fn budget_override(&self) -> Option<&str> {
        match self {
            Self::Finding {
                budget_override, ..
            }
            | Self::Resume {
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

/// Selection — replicates `scheduler.pick_next`'s priority order and
/// per-kind eligibility conditions.
///
/// Priority (`scheduler.pick_next`): a budget-overridden finding (any
/// category) jumps the queue -> flagged PR (oldest-outstanding reason
/// first) -> oldest merged PR pending follow-up review -> oldest
/// rechecking -> oldest queued fix -> resumable suspended work -> the
/// most stale-of-rotation job type for the least-recently-hunted enabled
/// repo (hunt if never cloned, else whichever of
/// `hunt/test_gap/dep_update/refactor` is oldest/never-run subject to
/// `cfg.scan_interval_days`; modernization gated by
/// `cfg.modernization_interval_days`). All types gated for one repo ->
/// the next-stalest repo is tried. None = nothing to do.
///
/// Each finding tier continues a suspended attempt at its own
/// (finding, kind) instead of starting that work over
/// ([`finding_pick`]).
///
/// Read-only but for retiring suspensions that can never be continued —
/// a chain past the give-up ceiling, or one whose working directory is
/// gone — which [`resume_plan`] does as it skips over them. Those writes
/// are best-effort precisely because this function is also the
/// `/api/summary` preview, which may hold a read-only handle — the
/// candidate is skipped either way, and the scheduler's own next cycle
/// records it.
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
            return Ok(Some(finding_pick(store, cfg, kind, f).await?));
        }
    }

    // (2)-(5) Normal finding-kind priorities, in this order.
    // attention/pending_harvest are oldest-first; list_findings is id
    // DESC, so last = oldest.
    for (kind, oldest) in [
        (FindingJobKind::Engage, attention.first()),
        (FindingJobKind::Harvest, pending_harvest.first()),
        (FindingJobKind::Recheck, rechecking.last()),
        (FindingJobKind::Fix, queued.last()),
    ] {
        if let Some(f) = oldest {
            return Ok(Some(finding_pick(store, cfg, kind, f).await?));
        }
    }

    // (6) Work already paid for: a suspended attempt whose transcript is
    // still on disk. This sits below every tier above it because those
    // are all a human waiting on a pull request — a flagged review, a
    // merged branch, a recheck they asked for, a fix they queued. A
    // resume is background work, and it outranks *starting* background
    // work because continuing costs the context at suspension (median
    // re-cache ratio 1.00 across 112 production events) where restarting
    // costs a flat ~37,000-token session floor plus every token already
    // spent, and advances no watermark to show for it.
    if let Some(c) = pick_resume(store, cfg, force_id).await? {
        return Ok(Some(c));
    }

    // (7) Repo rotation: enabled repos in staleness order — never-hunted
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

// ---------------------------------------------------------------------------
// Resume policy
// ---------------------------------------------------------------------------

/// Floor on the work half of any attempt's reservation.
///
/// The work half is whatever an attempt is budgeted beyond the context
/// it must load first, and it can come out tiny or negative: a chain
/// that has already outspent the per-kind typical `z` leaves
/// `z - chain_spent` below zero, and a negative budget for the work
/// still to do is not a small budget, it is a meaningless one. Floored
/// here at enough for the worker to do real work after loading, rather
/// than exactly enough to load and be killed again with nothing to
/// show, which would turn every resume into another suspension.
const MIN_PROGRESS_TOKENS: i64 = 25_000;

/// The system prompt and tool schemas a cold session loads before it
/// reads a single job-specific byte.
///
/// The smallest first call measured is 17,257 tokens. Kept below that
/// so it under-estimates: it feeds the floor arm of a `max()`, where
/// guessing high would refuse work the window could have funded.
const START_CONTEXT_FLOOR_TOKENS: i64 = 15_000;

/// The fraction of an attempt's budget that must go to work rather than
/// to loading context.
///
/// Every attempt pays to load its context before it can do anything —
/// a cold one its system prompt and tools, a resumed one its whole
/// transcript, re-sent at a measured median ratio of 1.00. That is
/// overhead; only what is left is work. Under a flat 25,000-token work
/// floor a resume with a 100,000-token transcript reserved 125,000, and
/// when the window had just that much room the attempt it admitted
/// spent 80% of its budget re-sending the transcript and 20% working.
/// Holding every start to at least this fraction of work makes the same
/// resume reserve 200,000, so it waits for a window that can fund an
/// attempt that is at least half work.
const MIN_START_EFFICIENCY: f64 = 0.5;

/// The least work budget worth paying `ctx` tokens of loading for.
///
/// Solves `work / (ctx + work) >= MIN_START_EFFICIENCY` for `work`, and
/// never below [`MIN_PROGRESS_TOKENS`]: a small context would otherwise
/// be funded for a few thousand tokens of work, too little to finish
/// anything.
fn min_useful(ctx: i64) -> i64 {
    // One expression rather than a named `eff / (1 - eff)` ratio: at 0.5
    // that ratio is exactly 1, so multiplying `ctx` by it and dividing
    // by it agree, and no reservation could show which one the code
    // does. Spelled out, a wrong operator anywhere here moves the
    // 100,000-token resume off 200,000.
    let work = ctx as f64 * MIN_START_EFFICIENCY / (1.0 - MIN_START_EFFICIENCY);
    MIN_PROGRESS_TOKENS.max(work.ceil() as i64)
}

/// Multiple of the per-kind typical cost at which a chain is abandoned.
///
/// Applied to what the chain spent OUTSIDE its single biggest attempt:
/// work that has burned three typical runs' worth on top of its one
/// most expensive try, and is still not finished, is not finishing.
/// Without a ceiling the chain resumes forever, each link cheap enough
/// to look reasonable — the exact loop this feature exists to end, only
/// slower.
const GIVE_UP_MULTIPLE: i64 = 3;

/// How many attempts one piece of work gets before the chain is
/// abandoned regardless of what it has cost.
///
/// The token arm below cannot bound a chain whose every attempt is
/// cheap: a suspension that resumes, does a little, and suspends again
/// runs forever without ever tripping a spend threshold. This arm is
/// also the one a reader can reason about, because it is expressed in
/// the unit the question is actually asked in — how many times have we
/// tried this.
const MAX_RESUME_ATTEMPTS: i64 = 4;

/// The whole prompt a resumed attempt gets.
///
/// Short by necessity, not by preference: the session already contains
/// the original playbook, the diff range, the suppression list and
/// everything the worker has done since. Re-sending it would re-send a
/// prompt the model is already looking at, for full price.
const RESUME_PROMPT: &str = "Continue the work you were doing in this session. You were interrupted; \
     pick up where you left off.";

/// What to reserve for a resumed attempt.
///
/// `ctx_at_suspension` is what the first call costs: a resumed session
/// re-establishes its whole context, measured median ratio 1.00 across
/// 112 production re-cache events. That is the overhead. The work half
/// is what is left of the per-kind typical estimate after everything
/// the chain has already spent, but never less than [`min_useful`] of
/// that context — a 100,000-token transcript reserves at least 200,000,
/// so the gate admits it only once at least half the attempt can be
/// work rather than re-sending the transcript.
fn resume_reservation(ctx_at_suspension: i64, z: i64, chain_spent: i64) -> i64 {
    ctx_at_suspension + (z - chain_spent).max(min_useful(ctx_at_suspension))
}

/// What to reserve for an attempt that starts cold.
///
/// The per-kind history, but never less than the cheapest cold start
/// that clears [`MIN_START_EFFICIENCY`]: [`START_CONTEXT_FLOOR_TOKENS`]
/// of fixed context plus [`min_useful`] of it, 40,000 with these
/// constants. History knows nothing about that floor and has collapsed
/// before: mis-metered rows once dragged the estimate to 1,876 tokens,
/// and the gate then started hunts with about 10,000 tokens of headroom
/// that died on their second call.
fn cold_reservation(history: i64) -> i64 {
    history.max(START_CONTEXT_FLOOR_TOKENS + min_useful(START_CONTEXT_FLOOR_TOKENS))
}

/// What the budget gate will reserve if `c` runs, for the
/// `/api/summary` preview: a preview that reserved anything else would
/// show a budget decision the scheduler is not going to make.
///
/// Unlike the gate, a failed history read is an error here rather than
/// a zero — the preview reports it instead of guessing.
pub async fn candidate_reservation(
    store: &Store,
    cfg: &Config,
    c: &Candidate,
) -> anyhow::Result<i64> {
    Ok(match c {
        Candidate::Resume { plan, .. } => plan.anticipated,
        Candidate::Repo { .. } | Candidate::Finding { .. } => {
            cold_reservation(anticipated_tokens(store, cfg, c.repo_id(), c.job_kind()).await?)
        }
    })
}

/// The resume tier of [`pick_next`]: the newest suspension that is still
/// worth continuing, or `None`.
///
/// Candidates are walked newest-first; [`resume_plan`] decides each one
/// and retires those that can never be continued, so the walk moves on
/// to the next in the same cycle — which is what "fall through to
/// normal selection" means.
async fn pick_resume(
    store: &Store,
    cfg: &Config,
    force_id: Option<i64>,
) -> anyhow::Result<Option<Candidate>> {
    for job in store.list_resumable_jobs().await? {
        if force_id.is_some_and(|rid| job.repo_id != rid) {
            continue;
        }
        if let Some(plan) = resume_plan(store, cfg, job).await? {
            return Ok(Some(Candidate::Resume {
                label: (!plan.repo.is_empty()).then(|| plan.repo.clone()),
                budget_override: None,
                plan: Box::new(plan),
            }));
        }
    }
    Ok(None)
}

/// A finding tier's pick: continue the suspended attempt at this same
/// (finding, kind) when one is resumable, otherwise start the work.
///
/// Without this the finding tiers, which outrank the resume tier, never
/// see the suspension: they start the work fresh — paying the session
/// floor plus everything the suspended attempt already spent. The
/// replacement keeps the tier's position, label and budget override;
/// only the plan differs.
async fn finding_pick(
    store: &Store,
    cfg: &Config,
    kind: FindingJobKind,
    f: &Finding,
) -> anyhow::Result<Candidate> {
    let fresh = finding_candidate(kind, f);
    let job_kind = JobKind::from(kind);
    for job in store.list_resumable_jobs().await? {
        if job.finding_id != Some(f.id) || job.kind != job_kind {
            continue;
        }
        if let Some(plan) = resume_plan(store, cfg, job).await? {
            return Ok(Candidate::Resume {
                label: fresh.label().map(str::to_owned),
                budget_override: fresh.budget_override().map(str::to_owned),
                plan: Box::new(plan),
            });
        }
    }
    Ok(fresh)
}

/// How to continue one resumable job, or `None` when it cannot be.
///
/// Checks the two things the database cannot know. The chain's
/// workspace must still hold its tree, and the transcript must live in
/// its session directory — a resumed worker continues a conversation, not
/// a filesystem, so pointing it at a tree that has since gone would have
/// it edit files that are not there. A row from before per-chain
/// workspaces fails this too: its transcript and tree are in the old
/// layout, and its row has no pinned commit. And the chain must be under
/// the give-up ceiling.
///
/// Failing either is permanent, so the job is retired here rather than
/// skipped. A skip leaves the row `suspended`: it is offered again every
/// cycle and never leaves the table. Refusing from inside the executor
/// would be worse — this tier outranks repo rotation, so the same
/// hopeless job would be picked every cycle, starving every rotation
/// kind. Retiring it takes it out of the walk in this same cycle.
async fn resume_plan(store: &Store, cfg: &Config, job: Job) -> anyhow::Result<Option<ResumePlan>> {
    // Non-NULL by the query's own filter; a row that lost its path
    // between the read and here is simply not resumable.
    let Some(session_file) = job.session_file.as_deref().map(PathBuf::from) else {
        return Ok(None);
    };
    let Some(repo) = store.get_repo_by_id(job.repo_id).await? else {
        return Ok(None);
    };
    let origin_job_id = store.resume_origin_job(job.id).await?;
    let workspace = crate::workspace::Workspace::for_chain(
        &cfg.work_root,
        Path::new(&repo.path),
        origin_job_id,
    );
    let pinned_sha = store.pinned_sha(job.id).await?;
    let pinned_sha = match pinned_sha {
        Some(sha) if workspace.tree.is_dir() && session_file.starts_with(&workspace.session) => sha,
        _ => {
            // The transcript names files in that tree, so no later cycle
            // can resume it either.
            let msg = format!(
                "resume {} {}: job {} retired, workspace {} is gone",
                job.kind,
                repo.name,
                job.id,
                workspace.root.display()
            );
            // Best-effort, like the give-up retirement below.
            let _ = store
                .retire_suspended_job(job.id, JobState::Killed, Some("workdir-gone"), &msg)
                .await;
            let _ = store
                .log_event("resume", &msg, Some(job.id), job.finding_id)
                .await;
            return Ok(None);
        }
    };

    let z = anticipated_tokens(store, cfg, job.repo_id, job.kind)
        .await
        .unwrap_or(0);
    let chain = store.resume_chain_stats(job.id).await?;
    let chain_spent = chain.total;
    // Two independent ways a chain runs out of road, and neither
    // implies the other: too many tries, or too much spent on the
    // tries other than the biggest one.
    //
    // The biggest attempt is set aside because one enormous attempt
    // is evidence about the SIZE OF THE JOB, not about the chain
    // being stuck — judging by it would retire big-but-healthy work
    // on its first resume. What the chain spent BESIDES that
    // attempt is the part that says continuing is not getting
    // anywhere.
    //
    // Subtracting rather than multiplying it is forced, not
    // stylistic: `chain_spent <= attempts * largest`, so any
    // threshold of the form `k * largest` is unreachable below
    // `attempts = k + 1` and would be decoration at k = 3.
    //
    // Both arms leave a chain of one alone. A first suspension is a
    // single attempt whose spend is, by definition of a cap kill, at
    // least its own cap; retiring on that would end the work before
    // resume had been tried even once, which is the opposite of what
    // this feature is for.
    let excess = chain_spent - chain.max_single;
    let too_many = chain.attempts >= MAX_RESUME_ATTEMPTS;
    let too_costly = chain.attempts >= 2 && excess > GIVE_UP_MULTIPLE * z;
    if too_many || too_costly {
        let why = if too_many {
            format!("{} attempts, the limit", chain.attempts)
        } else {
            format!("{excess} tok outside its largest attempt > {GIVE_UP_MULTIPLE}x {z} typical")
        };
        let msg = format!(
            "resume {} {}: giving up after {chain_spent} tok across the chain ({why})",
            job.kind, repo.name
        );
        // Best-effort: the summary preview runs this same function
        // over a read-only handle. Either way the candidate is
        // skipped, so a failed write only defers the record.
        let _ = store
            .retire_suspended_job(job.id, JobState::Failed, Some("give-up"), &msg)
            .await;
        let _ = store
            .log_event("resume", &msg, Some(job.id), job.finding_id)
            .await;
        return Ok(None);
    }

    // An unreadable transcript, or one with no usage record at all,
    // leaves the first call's cost unknown. `z` is the only other
    // estimate of this work that exists, so it stands in — better a
    // per-kind typical than a zero that would reserve nothing and
    // let the ramp grant a job it cannot afford.
    let ctx = crate::backends::omp_scavenge::harness::ctx_at_suspension(&session_file).unwrap_or(z);
    let anticipated = resume_reservation(ctx, z, chain_spent);

    // No "resuming" event here: this function is also the
    // /api/summary preview, which polls every few seconds, and an
    // event per poll would bury the log. `run_cycle_inner` logs it
    // when the cycle actually acts on the candidate.

    Ok(Some(ResumePlan {
        kind: job.kind,
        repo_id: job.repo_id,
        repo: repo.name,
        finding_id: job.finding_id,
        predecessor_id: job.id,
        origin_job_id,
        session_file,
        workspace,
        pinned_sha,
        anticipated,
        ctx,
        typical: z,
        chain_spent,
    }))
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
use crate::workspace::{TreeSpec, Workspace};
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
///
/// A cap kill that left a transcript behind is a PAUSE, not a failure.
/// The worker ran out of window headroom mid-thought and the record of
/// that thought is still on disk, so the work can be continued for the
/// price of re-caching it — measured median ratio 1.00 across 112
/// production re-cache events — instead of redone for a flat
/// ~37,000-token session floor plus everything already paid for. Calling
/// that outcome `killed` is what let one repo run ten consecutive hunts
/// over an identical diff range for 1.48M tokens, eight of them finding
/// nothing: a killed job advances no watermark, so the same work was
/// re-selected from scratch every cycle.
///
/// The session file is load-bearing, not incidental: resuming means
/// handing omp one exact path, and a cap kill with no transcript has
/// nothing to hand it.
///
/// Every other `killed_reason` stays `Killed` and is never resumed.
/// Wallclock above all: an unbounded overrun is the runaway signature,
/// and continuing a runaway only buys it more wall clock.
///
/// `resume-unavailable` is `Failed` rather than `Killed` because nothing
/// was spawned — there was no run to kill, the attempt could not start.
fn job_state(rr: &RunResult) -> JobState {
    match rr.killed_reason.as_deref() {
        Some("cap") if rr.session_file.is_some() => JobState::Suspended,
        Some("resume-unavailable") => JobState::Failed,
        Some(_) => JobState::Killed,
        None if rr.exit_code == Some(0) => JobState::Done,
        None => JobState::Failed,
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
///
/// `resume` is the plan this attempt ran under, if it was a resume. It
/// is here rather than in each executor because every executor funnels
/// through this one call, and the one outcome that needs it —
/// `resume-unavailable` — is identical for all six.
pub async fn record_job(
    store: &Store,
    job_id: i64,
    rr: &RunResult,
    model: Option<&str>,
    resume: Option<&ResumePlan>,
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

    // The transcript this attempt was to continue is gone, so nothing
    // ran. The predecessor is retired `killed`: leaving it `suspended`
    // would offer the same missing session every cycle forever.
    //
    // Deliberately NOT restarting the work cold in this same cycle. omp
    // treats an unresolvable `--resume` path as permission to start a
    // FRESH session, write it at that path and exit 0, which is
    // indistinguishable downstream from a real continuation — so
    // "cannot resume" quietly becoming "start cold here" would pay a
    // full session floor while believing it had continued. The next
    // cycle picks the work up through normal selection, cold and
    // knowing it.
    if let Some(plan) = resume
        && rr.killed_reason.as_deref() == Some("resume-unavailable")
    {
        let msg = format!(
            "resume {} {}: session gone, predecessor job {} marked killed",
            plan.kind, plan.repo, plan.predecessor_id
        );
        store
            .retire_suspended_job(plan.predecessor_id, JobState::Killed, None, &msg)
            .await?;
        let _ = store
            .log_event("error", &msg, Some(job_id), plan.finding_id)
            .await;
    }
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

/// Clone the repo if it is not cloned yet, then fetch.
///
/// Fetch only — never checkout or pull. The clone is the object store
/// every chain's worktree is added from, not a working tree anyone runs
/// in: moving its HEAD would do nothing for the trees, and moving the
/// trees is exactly what must never happen under a suspended chain.
/// Returns Ok(()) on success, Err with an error summary on failure.
async fn sync_repo(
    store: &Store,
    repo_url: &str,
    rpath: &Path,
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
    let rps = rpath.to_string_lossy().to_string();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(&["git", "-C", &rps, "fetch", "origin"], 600)
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("{log_prefix}: git fetch origin failed: {tail}"),
                None,
                finding_id,
            )
            .await;
        return Err(format!("git fetch origin failed: {tail}"));
    }
    Ok(())
}

/// Budget gate: check with backend, return the grant or a denied summary.
enum BudgetDecision {
    /// The token bound to enforce — `None` for a job the ramp put no
    /// ceiling on, which then runs under `maxWallS` alone — and the
    /// anticipated cost the ramp reserved to grant it. The job row records
    /// the latter so the inflight reservation can read it back while the
    /// job runs.
    Approved {
        cap: Option<i64>,
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
    use_override: bool,
    log_prefix: &str,
    finding_id: Option<i64>,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<BudgetDecision> {
    // A resumed attempt costs its context back plus whatever is left of
    // the per-kind typical, not the per-kind typical on its own: the
    // first call re-establishes a transcript that may be far larger than
    // a cold session's floor. Reserving `z` for it would under-reserve
    // by exactly the amount that makes resuming worth doing.
    //
    // Both reservations are floored so that loading context is at most
    // half of what an admitted attempt pays for (`MIN_START_EFFICIENCY`).
    // The floor only decides WHETHER the gate admits the job: the cap a
    // granted job runs under is the window's headroom computed without
    // this job's own reservation, so a larger reservation never shrinks
    // it. What it does is refuse a window too tight to fund the attempt
    // as mostly work, instead of starting one that spends its budget
    // loading context.
    let anticipated = match resume {
        Some(plan) => plan.anticipated,
        None => cold_reservation(
            anticipated_tokens(store, cfg, repo_id, kind)
                .await
                .unwrap_or(0),
        ),
    };
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
        } => Ok(BudgetDecision::Approved {
            // Verbatim: the ramp's headroom IS the budget for this job,
            // and it is the only token bound. A config constant beside it
            // could only ever be a second, static guess at the same
            // quantity — and the one that shipped had drifted below what
            // the jobs it governed cost, killing them on its own.
            cap: *backend_cap,
            anticipated,
        }),
    }
}

/// The chain workspace a job runs in: for a resume, the chain's own,
/// untouched; for a cold job, a new one — after first releasing the trees
/// of the chains its `create_job` superseded.
///
/// Called after the budget gate and after `create_job`, never before: a
/// job that is denied or refused has no workspace, so there is nothing to
/// clean up on those paths. The supersession release has to come first
/// because a superseded fix chain holds the same branch checked out, and
/// git refuses one branch in two worktrees.
///
/// `Ok(Err(summary))`: the tree could not be made. The job is recorded
/// `failed` with the reason, and [`crate::workspace::create`] has already
/// removed whatever it made.
async fn open_workspace(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    created: &CreatedJob,
    resume: Option<&ResumePlan>,
    spec: TreeSpec,
    kind: JobKind,
    log_prefix: &str,
    finding_id: Option<i64>,
) -> anyhow::Result<Result<(Workspace, String), CycleSummary>> {
    if let Some(plan) = resume {
        return Ok(Ok((plan.workspace.clone(), plan.pinned_sha.clone())));
    }
    let clone = Path::new(&repo.path);
    for &old in &created.superseded {
        let origin = store.resume_origin_job(old).await?;
        let stale = Workspace::for_chain(&cfg.work_root, clone, origin);
        crate::workspace::release_if_idle(store, &stale).await?;
    }
    let ws = Workspace::for_chain(&cfg.work_root, clone, created.id);
    let made = {
        let ws = ws.clone();
        tokio::task::spawn_blocking(move || crate::workspace::create(&ws, &spec)).await?
    };
    match made {
        Ok(sha) => {
            store.set_pinned_sha(created.id, &sha).await?;
            Ok(Ok((ws, sha)))
        }
        Err(why) => {
            let note = format!("workspace not created: {why}");
            store.fail_unstarted_job(created.id, &note).await?;
            let _ = store
                .log_event(
                    "error",
                    &format!("{log_prefix}: job {} {note}", created.id),
                    Some(created.id),
                    finding_id,
                )
                .await;
            Ok(Err(CycleSummary {
                kind: Some(kind),
                repo: Some(repo.name.clone()),
                finding_id,
                job_id: Some(created.id),
                state: Some(JobState::Failed),
                failure: Some(note),
                ..Default::default()
            }))
        }
    }
}

/// Release a chain's tree once its job has ended, unless the job table
/// says the chain is still `running` or `suspended` — a suspension is a
/// budget pause, and its tree is the state the resume continues.
///
/// Best effort: a tree that could not be released is picked up by the
/// sweep before the next cycle.
async fn close_workspace(store: &Store, ws: &Workspace) {
    if let Err(e) = crate::workspace::release_if_idle(store, ws).await {
        tracing::warn!("releasing {}: {e}", ws.tree.display());
    }
}

/// Retire a suspended attempt whose work concluded anyway.
///
/// A worker can finish the finding's business before the cap stops it —
/// leave a verdict file, or commit enough for the scheduler to open the
/// pull request. The finding then leaves the status the kind acts on, so
/// no resume could ever continue the chain, yet it would stay `suspended`
/// and be offered by the resume tier every cycle. Retiring it here makes
/// the chain terminal, so its tree is released with the job instead of
/// kept for a resume that cannot come. `killed_reason` is left alone: the
/// attempt really did stop on `cap`.
async fn retire_concluded(store: &Store, job: i64, outcome: &str) {
    let msg =
        format!("job {job} suspended after its work concluded ({outcome}); nothing to resume");
    let _ = store
        .retire_suspended_job(job, JobState::Killed, None, &msg)
        .await;
}

/// Run a hunt job (`scheduler.run_hunt`).
///
/// `resume` continues a suspended attempt: no sync, the diff range ending
/// at the chain's pinned commit, the same ingest and the same watermark
/// rule, but the worker is handed its own transcript and a one-line
/// "carry on" instead of a fresh playbook.
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
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    // Git's empty tree — the implicit parent of all root commits.
    // Using this as diff base includes the root commit itself.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
    let db = &repo.default_branch;

    // The commit this chain reviews up to. Cold: the freshly fetched tip
    // of the default branch, which the chain's tree is created at below.
    // Resume: the commit the chain's tree was created at, however far the
    // default branch has moved since — nothing is fetched or checked out
    // for a resume. Both the diff range and the watermark written on Done
    // come from this, so the watermark always names the commit the worker
    // actually reviewed and never marks as hunted a commit that was not in
    // the range.
    let head = if let Some(plan) = resume {
        plan.pinned_sha.clone()
    } else {
        sync_repo(store, &repo.url, &rpath, &format!("hunt {rname}"), None)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let clone = rpath.clone();
        let rev = format!("origin/{db}");
        let tip =
            tokio::task::spawn_blocking(move || crate::workspace::resolve(&clone, &rev)).await?;
        let Some(tip) = tip else {
            let _ = store
                .log_event(
                    "error",
                    &format!("hunt {rname}: origin/{db} does not resolve"),
                    None,
                    None,
                )
                .await;
            anyhow::bail!("origin/{db} does not resolve");
        };
        tip
    };
    let rp_str = rpath.to_string_lossy().to_string();

    let last = repo
        .last_hunt_sha
        .as_deref()
        .map(std::borrow::ToOwned::to_owned);
    let last_full = repo.last_full_hunt_at;
    let rehunt_interval_ms = cfg.hunt_rehunt_days * 86_400_000;
    // Both of the decisions below answer "should this work START", and
    // on a resume that question was settled in an earlier cycle. Firing
    // the periodic full re-hunt here would re-scope work already in
    // flight, and the no-new-commits skip would abandon a live session.
    let rehunt_due =
        resume.is_none() && last_full.is_some_and(|lf| (now_ms() - lf) > rehunt_interval_ms);
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
    if resume.is_none() && last.as_deref() == Some(&head) {
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
        false,
        &format!("hunt {rname}"),
        None,
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let created = match store
        .create_job(
            RepoJobKind::Hunt.into(),
            rid,
            None,
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(RepoJobKind::Hunt.into(), Some(rname), None, e),
    };
    let job = created.id;
    let (ws, pinned) = match open_workspace(
        store,
        cfg,
        repo,
        &created,
        resume,
        TreeSpec::Detached { at: head.clone() },
        RepoJobKind::Hunt.into(),
        &format!("hunt {rname}"),
        None,
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
    };
    // A resumed worker is still writing to the path its ORIGINAL prompt
    // named, so the attempt that ingests has to look there. Using this
    // job's own id would find nothing, advance no watermark, and put the
    // same work back in the rotation next cycle.
    let out_job = resume.map_or(job, |r| r.origin_job_id);
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("job{out_job}.findings.json"));
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
    let prompt = match resume {
        Some(_) => RESUME_PROMPT.to_owned(),
        None => playbooks::build_hunt_prompt(
            &cfg.root,
            repo,
            &ws.tree,
            &diff_range,
            &scope_note,
            &suppressions,
            &known,
            &out_path,
            cfg.hunt_max_findings,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("hunt");
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = record_job(store, job, &rr, model, resume)
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
            // The chain's pinned commit, never the clone's current tip:
            // see where `head` is resolved.
            let _ = store.set_last_hunt(rid, &pinned).await;
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
    close_workspace(store, &ws).await;
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
    resume: Option<&ResumePlan>,
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
    // Never on a resume: a resumed chain continues in its own tree, and
    // nothing is fetched for it.
    if resume.is_none() {
        sync_repo(
            store,
            &repo.url,
            &rpath,
            &format!("recheck #{fid}"),
            Some(fid),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Recheck.into(),
        override_mode.is_some(),
        &format!("recheck #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let created = match store
        .create_job(
            FindingJobKind::Recheck.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
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
    let job = created.id;
    let (ws, _pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
        resume,
        TreeSpec::Detached {
            at: format!("origin/{}", repo.default_branch),
        },
        FindingJobKind::Recheck.into(),
        &format!("recheck #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
    };
    let out_path = cfg.work_root.join("out").join(format!("recheck{fid}.json"));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));
    // Only on a cold run: the verdict file is finding-keyed, so on a
    // resume this path is the one the continuing worker was told to
    // write, and deleting it would discard a verdict it may already
    // have produced before the suspension.
    if resume.is_none() {
        // Remove stale output from a previous crashed attempt — a
        // leftover verdict file would be read as this attempt's result.
        let _ = std::fs::remove_file(&out_path);
    }

    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = match resume {
        Some(_) => RESUME_PROMPT.to_owned(),
        None => playbooks::build_recheck_prompt(
            &cfg.root,
            finding,
            &repo,
            &ws.tree,
            &out_path,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("hunt");
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = record_job(store, job, &rr, model, resume)
        .await
        .unwrap_or(JobState::Failed);
    // Released here rather than at each return below: everything after
    // this point reads the verdict under `out/`, never the tree.
    close_workspace(store, &ws).await;
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Recheck.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };
    if state == JobState::Suspended {
        // A suspension is a budget pause, not a failure: the worker ran out
        // of window headroom mid-work and its tree and transcript are kept
        // for the resume. Counting it toward the streak would turn three
        // pauses of one healthy recheck into "stuck" and reset the finding.
        // It stays rechecking, where the recheck tier continues it.
        let _ = store
            .log_event(
                "recheck",
                &format!(
                    "#{fid} suspended; worktree kept at {} for the resume",
                    ws.tree.display()
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("suspended".into());
        summary.worktree = Some(ws.tree.to_string_lossy().into_owned());
        if override_mode == Some("once") {
            let _ = store.set_budget_override(fid, None).await;
        }
        return Ok(summary);
    }

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
type AnalysisPromptBuilder = fn(
    &Path,
    &Repo,
    &Path,
    &str,
    &[Finding],
    &[Finding],
    &Path,
    i64,
    &str,
) -> anyhow::Result<String>;

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
    resume: Option<&ResumePlan>,
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
    // Never on a resume: a resumed chain continues in its own tree, and
    // nothing is fetched for it.
    if resume.is_none() {
        sync_repo(store, &repo.url, &rpath, &format!("{kind} {rname}"), None)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        rid,
        JobKind::from(kind),
        false,
        &format!("{kind} {rname}"),
        None,
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };
    let created = match store
        .create_job(
            kind.into(),
            rid,
            None,
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(kind.into(), Some(rname), None, e),
    };
    let job = created.id;
    let (ws, _pinned) = match open_workspace(
        store,
        cfg,
        repo,
        &created,
        resume,
        TreeSpec::Detached {
            at: format!("origin/{}", repo.default_branch),
        },
        kind.into(),
        &format!("{kind} {rname}"),
        None,
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
    };
    // The continuing worker still writes the path its original prompt
    // named; see `run_hunt`.
    let out_job = resume.map_or(job, |r| r.origin_job_id);
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("job{out_job}.{}.json", spec.out_plural));
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
    let prompt = match resume {
        Some(_) => RESUME_PROMPT.to_owned(),
        None => (spec.prompt_builder)(
            &cfg.root,
            repo,
            &ws.tree,
            spec.scope_note,
            &suppressions,
            &known,
            &out_path,
            cfg.hunt_max_findings,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("hunt");
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = record_job(store, job, &rr, model, resume)
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
    close_workspace(store, &ws).await;
    Ok(summary)
}

pub async fn run_test_gap(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &TEST_GAP_SPEC, backend, resume).await
}
/// Dependency scan. Renovate answers it for free when it is installed
/// and finds something; otherwise an AI worker does.
#[allow(
    clippy::too_many_lines,
    reason = "two alternative bodies for one job kind — the zero-token \
              Renovate path and the AI fallback — and the choice between \
              them is a single `match` on what the scan returned"
)]
pub async fn run_dep_update(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    // A resume continues a suspended AI worker, so the Renovate
    // shortcut below is not an option for it: that path spends no
    // tokens, creates no job row, and would leave the suspension
    // uncontinued and still resumable. `run_analysis_job` repeats the
    // clone check and the sync, so nothing is skipped by going straight
    // there.
    if resume.is_some() {
        return run_analysis_job(store, cfg, repo, &DEP_UPDATE_SPEC, backend, resume).await;
    }
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

    sync_repo(store, &repo.url, &rpath, "dep_update", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Try Renovate (zero tokens) — fall back to AI if unavailable. It
    // reads manifests, and the fetch-only clone's own files are never
    // updated, so it scans a throwaway tree at the fetched tip instead.
    let candidates = tokio::task::spawn_blocking({
        let work_root = cfg.work_root.clone();
        let rp = rpath.clone();
        let rn = repo.name.clone();
        let rid = repo.id;
        let tip = format!("origin/{}", repo.default_branch);
        move || {
            let scan = match crate::workspace::ScanTree::create(&work_root, &rp, rid, &tip) {
                Ok(scan) => scan,
                Err(e) => {
                    tracing::warn!("dep_update {rn}: {e}");
                    return None;
                }
            };
            crate::dep_scan::scan_repo(scan.path(), &rn, 120)
        }
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
            run_analysis_job(store, cfg, repo, &DEP_UPDATE_SPEC, backend, resume).await
        }
    }
}
pub async fn run_refactor(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &REFACTOR_SPEC, backend, resume).await
}
pub async fn run_modernize(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &MODERNIZATION_SPEC, backend, resume).await
}
pub async fn run_standards(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &STANDARDS_SPEC, backend, resume).await
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
    resume: Option<&ResumePlan>,
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
    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Fix.into(),
        override_mode.is_some(),
        &format!("fix #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let created = match store
        .create_job(
            FindingJobKind::Fix.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(FindingJobKind::Fix.into(), Some(&repo.name), Some(fid), e),
    };
    let job = created.id;
    // The branch is per finding, so a superseded fix chain of this finding
    // holds it checked out; `open_workspace` releases that chain's tree
    // before adding this one.
    let (ws, _pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
        resume,
        TreeSpec::Branch {
            name: branch.clone(),
            at: format!("origin/{}", repo.default_branch),
        },
        FindingJobKind::Fix.into(),
        &format!("fix #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
    };
    let worktree = ws.tree.clone();

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
    // The session already holds the playbook, the finding and the branch
    // name; the builders above still run because their failure is a real
    // configuration fault worth surfacing on either path.
    let prompt = if resume.is_some() {
        RESUME_PROMPT.to_owned()
    } else {
        prompt
    };
    let model = cfg.model_for("fix");
    let rr = match backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.fix_max_wall_s,
            JobClass::Fix,
            resume.map(|r| r.session_file.as_path()),
        )
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
    let state = record_job(store, job, &rr, model, resume)
        .await
        .unwrap_or(JobState::Failed);
    let summary = 'post: {
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
            summary.outcome = Some("rejected".into());
            let _ = store
                .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
                .await;
            break 'post summary;
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
                            break 'post summary;
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
                                break 'post summary;
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
        let tail = crate::util::tail(&rr.stdout_tail, 300);
        if state == JobState::Suspended {
            // A suspension is a budget pause, not a failure: the worker ran
            // out of window headroom mid-work and its tree and transcript are
            // kept for the resume. Counting it toward the streak would turn
            // three pauses of one healthy fix into "stuck" and reject the
            // finding. Back to queued, where the fix tier continues it.
            let _ = store.set_finding_status(fid, FindingStatus::Queued).await;
            let _ = store
                .log_event(
                    "fix",
                    &format!(
                        "#{fid} suspended; worktree kept at {} for the resume. tail: {tail}",
                        worktree.display()
                    ),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("suspended".into());
            summary.worktree = Some(worktree.to_string_lossy().into_owned());
        } else {
            let streak = store.record_fix_attempt(fid, &failure).await.unwrap_or(1);
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
                let _ = store
                .log_event(
                    "fix",
                    &format!(
                        "#{fid} gave up after {streak} identical failures ({failure}). tail: {tail}"
                    ),
                    Some(job),
                    Some(fid),
                )
                .await;
                summary.outcome = Some("stuck".into());
                summary.failure = Some(failure);
                summary.attempts = Some(streak);
            } else {
                let _ = store.set_finding_status(fid, FindingStatus::Queued).await;
                let _ = store
                    .log_event(
                        "fix",
                        &format!("#{fid} incomplete ({failure}). tail: {tail}"),
                        Some(job),
                        Some(fid),
                    )
                    .await;
                summary.outcome = Some("requeued".into());
                summary.failure = Some(failure);
            }
        }
        if override_mode == Some("once") {
            let _ = store.set_budget_override(fid, None).await;
        }
        let _ = store
            .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
            .await;
        summary
    };
    if state == JobState::Suspended
        && let Some(outcome @ ("rejected" | "pr_open")) = summary.outcome.as_deref()
    {
        retire_concluded(store, job, outcome).await;
    }
    close_workspace(store, &ws).await;
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
    resume: Option<&ResumePlan>,
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
    // Fetch the PR head into the clone for a cold attempt; the tree is
    // added from it once the job exists. Never on a resume: the chain
    // continues in its own tree, at the head it was created at.
    if resume.is_none() {
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
    }

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Engage.into(),
        override_mode.is_some(),
        &format!("engage #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
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
            anyhow::bail!("PR/MR view failed");
        }
    };

    let created = match store
        .create_job(
            FindingJobKind::Engage.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(
                FindingJobKind::Engage.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };
    let job = created.id;
    let (ws, _pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
        resume,
        TreeSpec::PrHead {
            head_ref: head_ref.clone(),
        },
        FindingJobKind::Engage.into(),
        &format!("engage #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
    };
    let worktree = ws.tree.clone();

    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = match resume {
        Some(_) => RESUME_PROMPT.to_owned(),
        None => playbooks::build_engage_prompt(
            &cfg.root,
            finding,
            &worktree,
            &head_ref,
            &repo,
            &pr,
            &ps,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("fix");
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.fix_max_wall_s,
            JobClass::Fix,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = record_job(store, job, &rr, model, resume)
        .await
        .unwrap_or(JobState::Failed);
    let summary = 'post: {
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
                    summary.outcome = Some("withdraw-failed".into());
                    break 'post summary;
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
            summary.outcome = Some("withdrawn".into());
            break 'post summary;
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
            // Only a suspension keeps its tree; any other outcome ends the
            // chain and the tree is released below.
            let kept = if state == JobState::Suspended {
                format!("; worktree kept at {} for the resume", worktree.display())
            } else {
                String::new()
            };
            let _ = store
                .log_event(
                    "engage",
                    &format!("#{fid} incomplete ({fail}){kept}. tail: {tail}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("retry".into());
            summary.failure = Some(fail.clone());
            if override_mode == Some("once") {
                let _ = store.set_budget_override(fid, None).await;
            }
            break 'post summary;
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
        summary.outcome = Some("engaged".into());
        if override_mode == Some("once") {
            let _ = store.set_budget_override(fid, None).await;
        }
        summary
    };
    if state == JobState::Suspended
        && let Some(outcome @ ("withdrawn" | "withdraw-failed")) = summary.outcome.as_deref()
    {
        retire_concluded(store, job, outcome).await;
    }
    close_workspace(store, &ws).await;
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
    resume: Option<&ResumePlan>,
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
    // Fetch the default branch into the clone for a cold attempt; the
    // tree is added at it once the job exists. Never on a resume: the
    // chain continues in its own tree.
    if resume.is_none() {
        let db = repo.default_branch.clone();
        let rps = rpath.to_string_lossy().to_string();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(&["git", "-C", &rps, "fetch", "origin", &db], 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "harvest #{fid}: fetch {} failed: {tail}",
                        repo.default_branch
                    ),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("fetch failed: {tail}");
        }
    }

    let override_mode = finding.budget_override.as_deref().filter(|s| !s.is_empty());
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Harvest.into(),
        override_mode.is_some(),
        &format!("harvest #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
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
            anyhow::bail!("PR/MR view failed");
        }
    };

    let created = match store
        .create_job(
            FindingJobKind::Harvest.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(
                FindingJobKind::Harvest.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };
    let job = created.id;
    let (ws, _pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
        resume,
        TreeSpec::Detached {
            at: format!("origin/{}", repo.default_branch),
        },
        FindingJobKind::Harvest.into(),
        &format!("harvest #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
    };
    let worktree = ws.tree.clone();
    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = match resume {
        Some(_) => RESUME_PROMPT.to_owned(),
        None => playbooks::build_harvest_prompt(
            &cfg.root,
            finding,
            &worktree,
            &repo.default_branch,
            &repo,
            &pr,
            pr_number,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("fix");
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.fix_max_wall_s,
            JobClass::Fix,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = record_job(store, job, &rr, model, resume)
        .await
        .unwrap_or(JobState::Failed);

    // Follow-ups are read from the tree, so before it is released. The
    // release itself is state-checked: a suspended harvest keeps its tree
    // for the resume, where this used to drop it after every run.
    let followups = ingest_followups(store, repo.id, &worktree, fid, job, "harvest").await;
    close_workspace(store, &ws).await;

    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Harvest.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        pr_number: Some(pr_number),
        ingest: followups,
        ..Default::default()
    };
    if state == JobState::Suspended {
        // A suspension is a budget pause, not a failure: the worker ran out
        // of window headroom mid-work and its tree and transcript are kept
        // for the resume. Counting it toward the streak would turn three
        // pauses of one healthy harvest into "stuck" and give the PR up.
        // It stays unharvested, where the harvest tier continues it.
        let _ = store
            .log_event(
                "harvest",
                &format!(
                    "#{fid} suspended; worktree kept at {} for the resume",
                    worktree.display()
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("suspended".into());
        summary.worktree = Some(worktree.to_string_lossy().into_owned());
        if override_mode == Some("once") {
            let _ = store.set_budget_override(fid, None).await;
        }
        return Ok(summary);
    }
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
                FindingJobKind::Engage => run_engage(store, cfg, &finding, backend, None).await?,
                FindingJobKind::Harvest => run_harvest(store, cfg, &finding, backend, None).await?,
                FindingJobKind::Recheck => run_recheck(store, cfg, &finding, backend, None).await?,
                FindingJobKind::Fix => run_fix(store, cfg, &finding, backend, None).await?,
            }
        }
        Candidate::Repo { kind, repo_id, .. } => {
            let repo = store
                .get_repo_by_id(*repo_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))?;
            match kind {
                RepoJobKind::Hunt => run_hunt(store, cfg, &repo, backend, None).await?,
                RepoJobKind::TestGap => run_test_gap(store, cfg, &repo, backend, None).await?,
                RepoJobKind::DepUpdate => run_dep_update(store, cfg, &repo, backend, None).await?,
                RepoJobKind::Refactor => run_refactor(store, cfg, &repo, backend, None).await?,
                RepoJobKind::Modernization => {
                    run_modernize(store, cfg, &repo, backend, None).await?
                }
                RepoJobKind::Standards => run_standards(store, cfg, &repo, backend, None).await?,
            }
        }
        // A resume runs through the executor of the kind that was
        // suspended — so ingest, watermarks and PR handling are that
        // kind's own, unchanged. Logged here rather than in `pick_next`,
        // which the summary endpoint also calls on every poll.
        Candidate::Resume { plan, .. } => {
            let _ = store
                .log_event(
                    "resume",
                    &format!(
                        "resume {} {}: job {} -> reserving {} tok \
                         (ctx {} + max({} - {}, {}))",
                        plan.kind,
                        plan.repo,
                        plan.predecessor_id,
                        plan.anticipated,
                        plan.ctx,
                        plan.typical,
                        plan.chain_spent,
                        min_useful(plan.ctx)
                    ),
                    Some(plan.predecessor_id),
                    plan.finding_id,
                )
                .await;
            let resume = Some(plan.as_ref());
            match plan.kind {
                JobKind::Repo(kind) => {
                    let repo = store
                        .get_repo_by_id(plan.repo_id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("repo {} not found", plan.repo_id))?;
                    match kind {
                        RepoJobKind::Hunt => run_hunt(store, cfg, &repo, backend, resume).await?,
                        RepoJobKind::TestGap => {
                            run_test_gap(store, cfg, &repo, backend, resume).await?
                        }
                        RepoJobKind::DepUpdate => {
                            run_dep_update(store, cfg, &repo, backend, resume).await?
                        }
                        RepoJobKind::Refactor => {
                            run_refactor(store, cfg, &repo, backend, resume).await?
                        }
                        RepoJobKind::Modernization => {
                            run_modernize(store, cfg, &repo, backend, resume).await?
                        }
                        RepoJobKind::Standards => {
                            run_standards(store, cfg, &repo, backend, resume).await?
                        }
                    }
                }
                JobKind::Finding(kind) => {
                    // A finding-kind job always recorded its target, so a
                    // row without one is corrupt rather than merely odd.
                    let fid = plan
                        .finding_id
                        .ok_or_else(|| anyhow::anyhow!("resume of a {kind} job with no finding"))?;
                    let finding = store
                        .get_finding(fid)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("finding {fid} not found"))?;
                    match kind {
                        FindingJobKind::Engage => {
                            run_engage(store, cfg, &finding, backend, resume).await?
                        }
                        FindingJobKind::Harvest => {
                            run_harvest(store, cfg, &finding, backend, resume).await?
                        }
                        FindingJobKind::Recheck => {
                            run_recheck(store, cfg, &finding, backend, resume).await?
                        }
                        FindingJobKind::Fix => {
                            run_fix(store, cfg, &finding, backend, resume).await?
                        }
                    }
                }
            }
        }
    };

    // Starvation prevention for rotation kinds: bump the timestamp on
    // failed/killed/done-without-output/done-with-all-invalid so this
    // kind doesn't monopolise the rotation forever. This is SEPARATE from
    // run_analysis_job's success-gated timestamp (which governs this kind's
    // own retry cadence) — this block ensures OTHER kinds get a turn.
    //
    // Suspended is deliberately absent from `failed` below. A cap kill is
    // a pause, and re-selecting that work is no longer this bump's job:
    // the resume tier claims it by id at a priority above rotation. Were
    // it bumped here it would be counted as a turn taken, while the work
    // itself had not started. A chain that never finishes is retired by
    // the give-up ceiling, and the still-old timestamp is then the honest
    // record that this scan has not run.
    //
    // `is_analysis()` covers standards here where the Python twin's list
    // does not, because standards exists only in this daemon — the twin
    // has no such kind to starve.
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
