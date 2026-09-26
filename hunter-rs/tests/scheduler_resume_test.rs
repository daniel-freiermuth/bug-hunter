#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The resume policy: what the scheduler does with a suspended attempt.
//!
//! The mechanism (a `resumed_from` link, a resumable-jobs query, a chain
//! sum, an omp `--resume`) is tested elsewhere. What is tested here is
//! the policy laid over it — when a suspension is chosen, what budget it
//! is granted, when it is abandoned, and what happens when the
//! transcript it named has gone.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::backend::{Backend, JobClass, Outlook, Verdict};
use hunter::config::Config;
use hunter::domain::{FindingJobKind, FindingStatus, JobKind, JobState, RepoJobKind};
use hunter::scheduler::{
    Candidate, ResumePlan, pick_next, record_job, run_fix, run_harvest, run_hunt, run_recheck,
};
use hunter::server::{AppState, SchedulerHandle, router};
use hunter::store::{FindingInsert, Store};
use hunter::types::RunResult;
use hunter::workspace::Workspace;
use sqlx::SqlitePool;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, git};
use tower::util::ServiceExt;

/// The per-kind typical cost every test here is calibrated against.
///
/// Three completed hunts at the same cost, so `anticipated_tokens`
/// returns it whether it reads p50 (warm) or p90 (cold) — the tests are
/// about the resume arithmetic, not about cache warmth.
const Z: i64 = 100_000;

/// `input + cacheRead + cacheWrite` of the LAST usage record in the
/// transcript [`seed_session`] writes.
const CTX: i64 = 200_000;

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the seed pool and the `Store` opened on `path`, and SQLite's handles
/// are closed before the files go.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("sched-resume");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

/// Read-write Store: selection retires a suspension it can never
/// continue (past the give-up ceiling, or its working directory gone), so
/// the selection path under test writes.
async fn rw_store(path: &Path) -> Store {
    Store::connect(path).await.unwrap()
}

fn test_config(root: &Path) -> Config {
    Config {
        root: root.to_path_buf(),
        work_root: root.join("data"),
        db_path: root.join("hunter.db"),
        serve_port: 0,
        ui_dir: root.join("ui"),
        omp_bin: "omp".to_owned(),
        stale_after_s: 300.0,
        cache_ttl_s: 3600.0,
        poll_s: 2.0,
        session_grace_s: 120,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".to_owned(),
        hunt_max_wall_s: 1800,
        hunt_max_findings: 8,
        fix_max_wall_s: 2700,
        hunt_rehunt_days: 90,
        scan_interval_days: 1.0,
        modernization_interval_days: 30,
        standards_interval_days: 30,
    }
}

/// One enabled repo whose clone directory really exists.
///
/// It has to exist: a hunt runs in the clone, and a resume is only
/// offered when the directory the suspended worker was using is still
/// there.
async fn seed_repo(pool: &SqlitePool, dir: &TempDir) -> PathBuf {
    let clone = dir.path().join("repo-1");
    std::fs::create_dir_all(&clone).unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/alpha.git', ?1, 'github', 'main', 1, 1000)",
    )
    .bind(clone.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    clone
}

/// Three completed hunts, all costing [`Z`]. Also makes the repo "warm",
/// which the constant is chosen to be indifferent to.
async fn seed_history(pool: &SqlitePool) {
    seed_history_at(pool, Z).await;
}

/// Three completed hunts, all costing `tokens`, so the per-kind
/// estimate is `tokens` whichever percentile `anticipated_tokens` reads.
async fn seed_history_at(pool: &SqlitePool, tokens: i64) {
    for id in 1..=3_i64 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES (?1, 'hunt', 1, 'done', ?2, 1000, 2000)",
        )
        .bind(id)
        .bind(tokens)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A worker transcript whose context at suspension is [`CTX`], in the
/// workspace of chain 10.
fn seed_session(dir: &TempDir) -> PathBuf {
    seed_session_with_ctx(dir, CTX)
}

/// The workspace of the chain whose first job is `origin`, as a resume
/// expects to find it: a tree directory and a session directory. Returns
/// the session directory.
fn seed_workspace(dir: &TempDir, origin: i64) -> PathBuf {
    let root = test_config(dir.path())
        .work_root
        .join("jobs")
        .join(origin.to_string());
    std::fs::create_dir_all(root.join("tree")).unwrap();
    let session = root.join("session");
    std::fs::create_dir_all(&session).unwrap();
    session
}

/// A worker transcript in omp's session format.
///
/// Two assistant calls. The first is a small opening exchange; the
/// second is where the session had got to when it was suspended, and its
/// `input + cacheRead + cacheWrite` is `ctx`. Only that last record
/// should count: re-establishing a session costs the context it had
/// reached, not the sum of every call that built it.
fn seed_session_with_ctx(dir: &TempDir, ctx: i64) -> PathBuf {
    seed_session_in(dir, 10, ctx)
}

/// [`seed_session_with_ctx`] in the workspace of chain `origin`.
fn seed_session_in(dir: &TempDir, origin: i64, ctx: i64) -> PathBuf {
    let path = seed_workspace(dir, origin).join("session.jsonl");
    let opening = serde_json::json!({"message": {"role": "assistant", "usage":
        {"input": 500, "output": 200, "cacheRead": 0, "cacheWrite": 37_000}}});
    let noise = serde_json::json!({"message": {"role": "user", "content": "noise"}});
    let last = serde_json::json!({"message": {"role": "assistant", "usage":
        {"input": 1_000, "output": 900, "cacheRead": ctx - 11_000, "cacheWrite": 10_000}}});
    std::fs::write(&path, format!("{opening}\n{noise}\n{last}\n")).unwrap();
    path
}

/// Finding `id` on repo 1 whose pull request is open but asks for
/// nothing: no tier has work for it.
async fn seed_quiet_finding(pool: &SqlitePool, id: i64) {
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (?1, 'bug', 1, 'fp' || ?1, 'high', 0.9, 'bug ' || ?1, 'pr_open', \
                 1000, 1000)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// Finding `id` on repo 1, its pull request open and flagged for
/// attention: the engage tier's work.
async fn seed_flagged_finding(pool: &SqlitePool, id: i64) {
    seed_quiet_finding(pool, id).await;
    sqlx::query(
        "INSERT INTO pr_state \
         (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
         VALUES (?1, 1, 'OPEN', 'review_comments', 1000, 5000)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// A suspended finding-kind job on repo 1, with a transcript.
async fn seed_finding_suspension(
    pool: &SqlitePool,
    id: i64,
    kind: &str,
    finding_id: i64,
    session: &Path,
) {
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, finding_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at, pinned_sha) \
         VALUES (?1, ?2, 1, ?3, 'suspended', ?4, 'cap', 40000, 1000, 2000, 'pinned')",
    )
    .bind(id)
    .bind(kind)
    .bind(finding_id)
    .bind(session.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
}

/// The `resume` events logged against job `id`, oldest first.
async fn resume_events(pool: &SqlitePool, id: i64) -> Vec<(String, Option<i64>)> {
    sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT message, finding_id FROM events WHERE kind = 'resume' AND job_id = ?1 \
         ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// One suspended hunt on repo 1, with a transcript and a measured cost.
async fn seed_suspension(pool: &SqlitePool, id: i64, session: &Path, tokens: i64) {
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at, pinned_sha) \
         VALUES (?1, 'hunt', 1, 'suspended', ?2, 'cap', ?3, 1000, 2000, 'pinned')",
    )
    .bind(id)
    .bind(session.to_string_lossy().to_string())
    .bind(tokens)
    .execute(pool)
    .await
    .unwrap();
}

/// A chain of linked attempts at one piece of work, oldest first, with
/// `tokens[i]` as each attempt's own cost. Returns the newest id.
///
/// Only the newest is resumable: a predecessor that already has a
/// successor row is being continued, and `list_resumable_jobs` filters
/// it out. So this is exactly the shape `pick_resume` sees when it has
/// to decide whether to continue a chain yet again.
async fn seed_chain(pool: &SqlitePool, session: &Path, tokens: &[i64]) -> i64 {
    let mut id = 9_i64;
    let mut previous: Option<i64> = None;
    for &t in tokens {
        id += 1;
        sqlx::query(
            "INSERT INTO jobs \
             (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
              resumed_from, started_at, finished_at, pinned_sha) \
             VALUES (?1, 'hunt', 1, 'suspended', ?2, 'cap', ?3, ?4, 1000, 2000, 'pinned')",
        )
        .bind(id)
        .bind(session.to_string_lossy().to_string())
        .bind(t)
        .bind(previous)
        .execute(pool)
        .await
        .unwrap();
        previous = Some(id);
    }
    id
}

async fn job_row(pool: &SqlitePool, id: i64) -> (String, Option<String>, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT state, killed_reason, notes FROM jobs WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn resume_plan_of(c: Option<Candidate>) -> ResumePlan {
    match c {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    }
}

// -- the reservation ---------------------------------------------------------

/// A resumed attempt reserves its context back, plus what is left of the
/// per-kind typical after everything the chain has already spent.
///
/// The context term is the point: the first call of a resumed session
/// re-establishes the whole transcript, measured at a median ratio of
/// 1.00 across 112 production re-cache events. Reserving only the
/// leftover estimate would under-reserve by exactly the amount that
/// makes resuming cheaper than restarting.
///
/// The context is small here so that the leftover estimate, not the
/// work floor, is the larger work term.
#[tokio::test]
async fn resume_reserves_context_plus_the_remaining_estimate() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let ctx = 20_000;
    let session = seed_session_with_ctx(&dir, ctx);
    let spent = 40_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(plan.predecessor_id, 10);
    assert_eq!(plan.origin_job_id, 10, "a first resume continues the root");
    assert_eq!(plan.session_file, session);
    assert_eq!(
        plan.anticipated,
        ctx + (Z - spent),
        "ctx {ctx} + max({Z} - {spent}, 25_000)"
    );
}

/// A resume reserves at least as much work as it re-sends context, even
/// once the chain has spent past the per-kind typical.
///
/// Re-sending the transcript is pure overhead. Under a flat 25,000
/// work floor, a 100,000-token transcript whose chain had exhausted the
/// typical reserved 125,000, and a window with just that much room
/// admitted an attempt that spent 80% of its budget re-sending and 20%
/// working. Reserving 200,000 makes the gate wait for a window that can
/// fund an attempt that is at least half work.
#[tokio::test]
async fn a_resume_reserves_at_least_as_much_work_as_it_resends_context() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session_with_ctx(&dir, 100_000);
    // Over the typical, so `z - chain_spent` is negative, but well under
    // the give-up ceiling of 3x.
    let spent = Z + 30_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(plan.ctx, 100_000);
    assert_eq!(
        plan.anticipated, 200_000,
        "100_000 of context + at least 100_000 of work"
    );
}

/// An unreadable transcript leaves the first call's cost unknown, and
/// the per-kind typical is the only other estimate of this work that
/// exists. Reserving nothing would let the ramp grant a job whose very
/// first call it cannot afford.
#[tokio::test]
async fn resume_falls_back_to_the_per_kind_estimate_when_the_transcript_is_unreadable() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let missing = seed_workspace(&dir, 10).join("never-written.jsonl");
    let spent = 40_000;
    seed_suspension(&pool, 10, &missing, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(
        plan.ctx, Z,
        "the per-kind typical stands in for the context"
    );
    assert_eq!(
        plan.anticipated,
        2 * Z,
        "ctx {Z} + max({Z} - {spent}, as much work as context)"
    );
}

// -- the give-up ceiling -----------------------------------------------------

/// A chain that has been tried too many times is abandoned, however
/// cheap each attempt was.
///
/// This is the arm that actually fires in practice. A suspension that
/// resumes, does a little, and suspends again never trips a spend
/// threshold, so without a count the chain runs forever with every link
/// looking individually reasonable. Falling through to normal selection
/// is the other half: the cycle still does something useful.
#[tokio::test]
async fn a_chain_out_of_attempts_is_retired_and_the_cycle_falls_through() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Four cheap attempts: 40_000 all told, nowhere near 3x the 100_000
    // typical, so only the attempt count can end this.
    let newest = seed_chain(&pool, &session, &[10_000; 4]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&store, &cfg, None).await.unwrap();

    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::Hunt,
                ..
            })
        ),
        "must fall through to normal selection, got {picked:?}"
    );
    let (state, reason, notes) = job_row(&pool, newest).await;
    assert_eq!(state, JobState::Failed.as_str());
    assert_eq!(reason.as_deref(), Some("give-up"));
    let notes = notes.unwrap();
    assert!(
        notes.contains("40000 tok") && notes.contains("4 attempts"),
        "the record must name what the chain cost and how often it was tried, got {notes:?}"
    );
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "a retired chain must never be offered again"
    );
}

/// A chain whose attempts OTHER than its biggest have outspent the
/// per-kind typical three times over is abandoned.
///
/// The second arm, and the one that catches a chain burning real money
/// before it has been tried four times.
#[tokio::test]
async fn a_chain_that_outspends_the_typical_outside_its_biggest_attempt_is_retired() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Two attempts, so the count arm cannot fire: 800_000 spent, of
    // which 400_000 is outside the larger attempt, against 3x 100_000.
    let newest = seed_chain(&pool, &session, &[400_000, 400_000]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&store, &cfg, None).await.unwrap();

    assert!(
        matches!(picked, Some(Candidate::Repo { .. })),
        "must fall through to normal selection, got {picked:?}"
    );
    let (state, reason, _) = job_row(&pool, newest).await;
    assert_eq!(state, JobState::Failed.as_str());
    assert_eq!(reason.as_deref(), Some("give-up"));
}

/// A chain of ONE attempt is never retired, however expensive it was.
///
/// A first suspension is a single attempt whose spend is, by definition
/// of a cap kill, at least its own cap — so comparing it against a
/// multiple of the per-kind typical retires the work before resume has
/// been tried even once, which is the exact opposite of what this
/// feature is for. Live symptom: every hunt on one repo recorded
/// `failed`/`give-up` with 88,697 spent against an 80,493 cap, and no
/// job was ever resumed at all.
#[tokio::test]
async fn a_chain_of_one_attempt_is_never_retired() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Fifty times the typical, on a single attempt that has never been
    // continued. There is nothing here to conclude that continuing does
    // not work, because continuing has not been tried.
    seed_suspension(&pool, 10, &session, 50 * Z).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(plan.predecessor_id, 10);
    let (state, _, _) = job_row(&pool, 10).await;
    assert_eq!(state, JobState::Suspended.as_str());
}

/// One enormous attempt is evidence about the size of the JOB, not
/// about the chain being stuck, so the chain is judged on what it spent
/// besides that attempt.
///
/// Without setting it aside, the per-kind typical is the wrong
/// yardstick for this job and a big-but-healthy piece of work is retired
/// on its first resume — the whole chain here is over 3x the typical,
/// and all but 10,000 of it is one attempt.
#[tokio::test]
async fn the_biggest_attempt_is_set_aside_before_the_chain_is_judged() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[500_000, 10_000]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(plan.predecessor_id, newest);
    assert_eq!(plan.chain_spent, 510_000, "5x the typical, and still fine");
}

/// Exactly at the ceiling is still resumable: "more than three times the
/// typical" is the condition, and a chain that has spent precisely 3x
/// outside its biggest attempt has not passed it.
#[tokio::test]
async fn the_give_up_ceiling_is_exclusive() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[400_000, 3 * Z]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());
    assert_eq!(plan.predecessor_id, newest);
}

// -- selection order ---------------------------------------------------------

/// A resume outranks starting new background work, but not a human
/// waiting on a pull request.
///
/// A queued fix is a finding someone triaged and asked for; the resume
/// tier sits below it and above repo rotation.
#[tokio::test]
async fn a_queued_fix_outranks_a_resume_which_outranks_rotation() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (5, 'bug', 1, 'fp5', 'high', 0.9, 'queued bug', 'queued', 1000, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert_eq!(
        picked.as_ref().map(Candidate::target_id),
        Some(5),
        "the human-queued fix wins, got {picked:?}"
    );

    // Drop the fix and the same suspension beats repo rotation.
    sqlx::query("DELETE FROM findings WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());
    assert_eq!(plan.predecessor_id, 10);
}

/// A suspension whose chain tree is gone is retired, not merely
/// skipped, and never offered.
///
/// A resumed worker continues a conversation, not a filesystem: its
/// transcript refers to files in that tree, so no later cycle can
/// continue it either. Skipping it left the row `suspended` forever —
/// never resumed, never retired. Retiring at selection, in the same
/// walk, still keeps it from being offered again, which matters because
/// this tier outranks rotation.
#[tokio::test]
async fn a_suspension_whose_working_directory_is_gone_is_retired() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    let cfg = test_config(dir.path());
    let workspace = cfg.work_root.join("jobs").join("10");
    std::fs::remove_dir_all(workspace.join("tree")).unwrap();
    let store = rw_store(&path).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();

    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::Hunt,
                ..
            })
        ),
        "a suspension with no tree falls through to rotation, got {picked:?}"
    );
    let (state, reason, notes) = job_row(&pool, 10).await;
    assert_eq!(state, JobState::Killed.as_str());
    assert_eq!(reason.as_deref(), Some("workdir-gone"));
    let msg = format!(
        "resume hunt alpha: job 10 retired, workspace {} is gone",
        workspace.display()
    );
    assert_eq!(notes.as_deref(), Some(msg.as_str()));
    assert_eq!(resume_events(&pool, 10).await, vec![(msg, None)]);
}

/// A flagged pull request whose engage was cap-killed continues that
/// engage instead of starting a fresh one.
///
/// The finding tiers outrank the resume tier, so they are the ones that
/// must notice. Live: engage job 4358 suspended with a valid transcript;
/// nine minutes later the attention tier picked the same finding and
/// started a fresh engage for 92,294 tokens where resuming would have
/// re-cached about 75,000 — and the fresh attempt's worktree handling
/// removed the directory the suspended transcript refers to.
///
/// Two newer suspensions sit in front of the right one — an engage for
/// another finding and a fix for this one — so matching on either half
/// of (finding, kind) alone picks the wrong job.
#[tokio::test]
async fn a_flagged_pr_resumes_its_suspended_engage_instead_of_starting_fresh() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    seed_flagged_finding(&pool, 5).await;
    seed_quiet_finding(&pool, 6).await;
    for (id, kind, finding) in [(20, "engage", 5), (21, "engage", 6), (22, "fix", 5)] {
        let session = seed_session_in(&dir, id, CTX);
        seed_finding_suspension(&pool, id, kind, finding, &session).await;
    }
    let store = rw_store(&path).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();

    let Some(Candidate::Resume { label, plan, .. }) = picked else {
        panic!("expected a resume of the suspended engage, got {picked:?}");
    };
    assert_eq!(plan.predecessor_id, 20);
    assert_eq!(plan.kind, JobKind::from(FindingJobKind::Engage));
    assert_eq!(plan.finding_id, Some(5));
    assert_eq!(
        label.as_deref(),
        Some("bug 5"),
        "the tier keeps its own label"
    );
}

/// A budget-overridden finding with a suspended attempt resumes it and
/// keeps the override.
///
/// The override tier jumps the queue, so it must continue rather than
/// restart just like the tiers below it — and the resume still carries
/// the override, or the summary preview judges it by the normal budget
/// while the executor runs it under the prioritized one.
#[tokio::test]
async fn an_overridden_finding_resumes_its_suspended_attempt_under_the_override() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    let session = seed_session_in(&dir, 20, CTX);
    seed_flagged_finding(&pool, 5).await;
    sqlx::query("UPDATE findings SET budget_override = 'once' WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap().unwrap();

    assert_eq!(picked.budget_override(), Some("once"));
    assert_eq!(resume_plan_of(Some(picked)).predecessor_id, 20);
}

/// The same flagged pull request, but the suspended engage's tree is
/// gone: that suspension is retired and the tier starts fresh.
///
/// This is job 4358 after a fresh engage removed its worktree. It can
/// never be resumed, so retiring it on the first cycle that sees it is
/// the only way it leaves `suspended`.
#[tokio::test]
async fn a_flagged_pr_whose_suspended_engage_lost_its_worktree_starts_fresh() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    let session = seed_session_in(&dir, 20, CTX);
    let workspace = cfg.work_root.join("jobs").join("20");
    std::fs::remove_dir_all(workspace.join("tree")).unwrap();
    seed_flagged_finding(&pool, 5).await;
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();

    assert!(
        matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Engage,
                finding_id: 5,
                ..
            })
        ),
        "a suspension that cannot be continued must not block the fresh engage, got {picked:?}"
    );
    let (state, reason, _) = job_row(&pool, 20).await;
    assert_eq!(state, JobState::Killed.as_str());
    assert_eq!(reason.as_deref(), Some("workdir-gone"));
    assert_eq!(
        resume_events(&pool, 20).await,
        vec![(
            format!(
                "resume engage alpha: job 20 retired, workspace {} is gone",
                workspace.display()
            ),
            Some(5)
        )]
    );
}

// -- fresh work supersedes a suspension --------------------------------------

/// Starting a finding's work fresh retires that finding's suspended
/// attempt at the same kind.
///
/// Whatever path started it, the fresh attempt now owns the work, and
/// its worktree handling reclaims the directory the old transcript
/// refers to. A suspension left behind is never resumed and never
/// retired: it stays `suspended` forever. Other findings and other
/// kinds are different work and are left alone.
#[tokio::test]
async fn fresh_work_on_a_finding_supersedes_its_suspended_attempt() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let session = seed_session(&dir);
    seed_flagged_finding(&pool, 5).await;
    seed_quiet_finding(&pool, 6).await;
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    seed_finding_suspension(&pool, 21, "engage", 6, &session).await;
    seed_finding_suspension(&pool, 22, "fix", 5, &session).await;
    let store = rw_store(&path).await;

    let created = store
        .create_job(
            FindingJobKind::Engage.into(),
            1,
            Some(5),
            None,
            JobState::Running,
            Some(40_000),
            None,
        )
        .await
        .unwrap();
    let fresh = created.id;
    assert_eq!(
        created.superseded,
        vec![20],
        "the caller is told which chain to release before building its own"
    );

    let (state, reason, notes) = job_row(&pool, 20).await;
    assert_eq!(state, JobState::Killed.as_str());
    assert_eq!(reason.as_deref(), Some("superseded"));
    let msg = format!("resume engage alpha: job 20 superseded by fresh job {fresh}");
    assert_eq!(notes.as_deref(), Some(msg.as_str()));
    assert_eq!(resume_events(&pool, 20).await, vec![(msg, Some(5))]);
    for other in [21, 22] {
        assert_eq!(
            job_row(&pool, other).await.0,
            JobState::Suspended.as_str(),
            "job {other} is different work and must stay resumable"
        );
    }
}

/// Starting a repo-level kind fresh retires that repo's suspended
/// attempt at the same kind, and nothing else.
#[tokio::test]
async fn fresh_work_on_a_repo_supersedes_its_suspended_attempt() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', 'https://example.com/beta.git', '/nowhere', 'github', 'main', 1, \
                 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 40_000).await;
    for (id, kind, repo) in [(11, "test_gap", 1), (12, "hunt", 2)] {
        sqlx::query(
            "INSERT INTO jobs \
             (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
              started_at, finished_at) \
             VALUES (?1, ?2, ?3, 'suspended', ?4, 'cap', 40000, 1000, 2000)",
        )
        .bind(id)
        .bind(kind)
        .bind(repo)
        .bind(session.to_string_lossy().to_string())
        .execute(&pool)
        .await
        .unwrap();
    }
    let store = rw_store(&path).await;

    let fresh = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            None,
            JobState::Running,
            Some(40_000),
            None,
        )
        .await
        .unwrap()
        .id;

    let (state, reason, notes) = job_row(&pool, 10).await;
    assert_eq!(state, JobState::Killed.as_str());
    assert_eq!(reason.as_deref(), Some("superseded"));
    let msg = format!("resume hunt alpha: job 10 superseded by fresh job {fresh}");
    assert_eq!(notes.as_deref(), Some(msg.as_str()));
    assert_eq!(resume_events(&pool, 10).await, vec![(msg, None)]);
    for other in [11, 12] {
        assert_eq!(
            job_row(&pool, other).await.0,
            JobState::Suspended.as_str(),
            "job {other} is different work and must stay resumable"
        );
    }
}

/// A cycle forced onto one repo resumes that repo's suspension and no
/// other.
///
/// Forcing is the operator asking for work on THIS repo. Continuing
/// another repo's suspended job instead would spend the budget they
/// meant for this one on work they did not ask for, and report it as the
/// cycle they forced. Checked from both sides, so the answer cannot
/// depend on which suspension the query happens to list first.
#[tokio::test]
async fn a_forced_cycle_resumes_only_the_forced_repos_suspension() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    let beta = dir.path().join("repo-2");
    std::fs::create_dir_all(&beta).unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', 'https://example.com/beta.git', ?1, 'github', 'main', 1, 1000)",
    )
    .bind(beta.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at, pinned_sha) \
         VALUES (20, 'hunt', 2, 'suspended', ?1, 'cap', 10000, 1000, 2000, 'pinned')",
    )
    .bind(seed_session_in(&dir, 20, CTX).to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    for (forced, repo_id, suspended) in [("alpha", 1, 10), ("beta", 2, 20)] {
        let picked = pick_next(&store, &cfg, Some(forced)).await.unwrap();
        assert_eq!(
            picked.as_ref().map(Candidate::repo_id),
            Some(repo_id),
            "forcing {forced} must stay on {forced}, got {picked:?}"
        );
        assert_eq!(resume_plan_of(picked).predecessor_id, suspended);
    }
}

/// A resume is shown under its repo's name, as the fresh hunt it
/// replaces would be: the `/api/summary` preview displays the
/// candidate's label, and an unlabelled resume there reads as work on
/// nothing in particular.
#[tokio::test]
async fn a_resume_is_labelled_with_its_repos_name() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&store, &cfg, None).await.unwrap();

    assert!(
        matches!(picked, Some(Candidate::Resume { .. })),
        "{picked:?}"
    );
    assert_eq!(picked.as_ref().and_then(Candidate::label), Some("alpha"));
}

// -- the transcript that vanished --------------------------------------------

/// When the resume source has gone, the attempt fails and the
/// predecessor is retired.
///
/// Nothing was spawned, so `failed` is the honest state for the attempt.
/// The predecessor must leave `suspended`: leaving it there would offer
/// the same missing session every cycle forever. Its own
/// `killed_reason` is preserved — that attempt really did stop on `cap`,
/// and overwriting it with the successor's problem would lose the only
/// record of why the work stopped.
#[tokio::test]
async fn resume_unavailable_fails_the_attempt_and_retires_the_predecessor() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 40_000).await;
    let store = rw_store(&path).await;

    let attempt = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            None,
            JobState::Running,
            Some(260_000),
            Some(10),
        )
        .await
        .unwrap()
        .id;
    let plan = ResumePlan {
        kind: RepoJobKind::Hunt.into(),
        repo_id: 1,
        repo: "alpha".to_owned(),
        finding_id: None,
        predecessor_id: 10,
        origin_job_id: 10,
        session_file: session,
        workspace: hunter::workspace::Workspace::for_chain(
            &test_config(dir.path()).work_root,
            &dir.path().join("repo-1"),
            10,
        ),
        pinned_sha: "pinned".to_owned(),
        anticipated: 260_000,
        ctx: CTX,
        typical: Z,
        chain_spent: 40_000,
    };
    // What the harness returns when it refuses to hand omp a path it
    // cannot resolve: nothing spawned, no exit code, no transcript.
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("resume-unavailable".to_owned()),
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 0.0,
        stdout_tail: "cannot resume: session directory is gone".to_owned(),
        usage_delta: None,
    };

    let state = record_job(&store, attempt, &rr, None, Some(&plan))
        .await
        .unwrap();

    assert_eq!(
        state,
        JobState::Failed,
        "nothing ran, so nothing was killed"
    );
    let (attempt_state, _, _) = job_row(&pool, attempt).await;
    assert_eq!(attempt_state, JobState::Failed.as_str());

    let (pred_state, pred_reason, pred_notes) = job_row(&pool, 10).await;
    assert_eq!(pred_state, JobState::Killed.as_str());
    assert_eq!(
        pred_reason.as_deref(),
        Some("cap"),
        "the predecessor's own reason for stopping is not rewritten"
    );
    assert!(pred_notes.unwrap().contains("session gone"));
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "a missing transcript must not be retried forever"
    );
}

// -- executing a resumed attempt ---------------------------------------------

/// What the executor actually handed the backend.
#[derive(Debug)]
struct Seen {
    cwd: PathBuf,
    prompt: String,
    resume_from: Option<PathBuf>,
}

/// A backend that records its `run` arguments and stages a worker's
/// output at a path fixed when it is built.
///
/// `ScriptedBackend` only sees the worktree, and the three things a
/// resume changes about a run — the prompt, the resume source, and
/// which output file the worker is really writing — are invisible
/// through it.
struct RecordingBackend {
    seen: OnceLock<Seen>,
    writes: PathBuf,
    body: String,
}

impl RecordingBackend {
    fn new(writes: PathBuf, body: String) -> Self {
        Self {
            seen: OnceLock::new(),
            writes,
            body,
        }
    }

    fn seen(&self) -> &Seen {
        self.seen.get().expect("the backend was never run")
    }
}

#[async_trait::async_trait]
impl Backend for RecordingBackend {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let granted = Verdict::Granted {
            cap_tokens: None,
            reason: "test: always granted".to_owned(),
        };
        Ok(Outlook {
            normal: granted.clone(),
            prioritized: granted,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn run(
        &self,
        ws: &Workspace,
        prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        // `set` fails only if this backend ran twice, which no test
        // here does — each one builds its own.
        let _ = self.seen.set(Seen {
            cwd: ws.tree.clone(),
            prompt: prompt.to_owned(),
            resume_from: resume_from.map(Path::to_path_buf),
        });
        std::fs::write(&self.writes, &self.body).expect("stage worker output");
        Ok(RunResult {
            exit_code: Some(0),
            killed_reason: None,
            tokens_new: 30_000,
            calls: 4,
            session_file: Some(
                resume_from
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            duration_s: 1.0,
            stdout_tail: String::new(),
            usage_delta: None,
        })
    }
}

/// A real clone with a bare origin, registered as repo 1.
async fn cloned_repo(dir: &TempDir, pool: &SqlitePool) -> GitRepo {
    let repo = GitRepo::with_branch(dir, "feature");
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(repo.work.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    repo
}

/// [`cloned_repo`] plus history, a transcript and a suspended hunt whose
/// id is `pred`.
async fn executable_repo(dir: &TempDir, pool: &SqlitePool, pred: i64) -> (GitRepo, PathBuf) {
    let repo = cloned_repo(dir, pool).await;
    seed_history(pool).await;
    let session = seed_session_in(dir, pred, CTX);
    seed_suspension(pool, pred, &session, 40_000).await;
    let head = git(&repo.work, &["rev-parse", "HEAD"]);
    sqlx::query("UPDATE jobs SET pinned_sha = ?1 WHERE id = ?2")
        .bind(head.trim())
        .bind(pred)
        .execute(pool)
        .await
        .unwrap();
    (repo, session)
}

fn one_finding() -> String {
    serde_json::json!([{
        "fingerprint": "alpha:src/lib.rs:parse:1",
        "type": "bug",
        "file": "src/lib.rs",
        "line": 12,
        "bug_class": "boundary",
        "severity": "high",
        "confidence": 0.9,
        "summary": "off-by-one in the parser",
        "detail": "found before the suspension",
        "evidence_plan": "failing test first"
    }])
    .to_string()
}

/// A resumed hunt hands omp the predecessor's transcript and a one-line
/// continuation, and reads back the output file the ORIGINAL prompt
/// named.
///
/// The output path is the part that makes the whole feature work or not.
/// The playbook bakes `out/job<N>.findings.json` into the prompt, so a
/// continuing worker writes the first attempt's path forever. An
/// executor that looked at its own id would find nothing, ingest
/// nothing, and advance no watermark — putting the identical work back
/// in the rotation next cycle, which is the loop this feature exists to
/// end.
#[tokio::test]
async fn a_resumed_hunt_continues_the_session_and_ingests_the_origin_output_path() {
    let (dir, path, pool) = fresh_db().await;
    let (_repo, session) = executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = match pick_next(&store, &cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    };
    let backend = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );
    let row = store.get_repo_by_id(1).await.unwrap().unwrap();

    let summary = run_hunt(&store, &cfg, &row, &backend, Some(&plan))
        .await
        .unwrap();

    let seen = backend.seen();
    assert_eq!(
        seen.prompt,
        "Continue the work you were doing in this session. \
         You were interrupted; pick up where you left off.",
        "a resumed run must not re-send the playbook it is already looking at"
    );
    assert_eq!(seen.resume_from.as_deref(), Some(session.as_path()));
    assert_eq!(
        seen.cwd,
        cfg.work_root.join("jobs").join("10").join("tree"),
        "a resume runs in its chain's tree"
    );
    assert_eq!(
        summary.ingest.map(|i| i.inserted),
        Some(1),
        "the findings the continuing worker wrote must be ingested"
    );

    let (kind, resumed_from, estimated): (String, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT kind, resumed_from, estimated_tokens FROM jobs WHERE id = ?1")
            .bind(summary.job_id.unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, "hunt", "a resume runs as the kind that was suspended");
    assert_eq!(resumed_from, Some(10));
    assert_eq!(
        estimated,
        Some(plan.anticipated),
        "the row records the resume's reservation, not a cold estimate"
    );

    let after = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert!(
        after.last_hunt_sha.is_some(),
        "a resume that completes advances the watermark like any other Done hunt"
    );
}

/// A resumed run does not sync the clone.
///
/// Fast-forwarding moves the tree out from under a session whose
/// transcript describes the old one — and `run_hunt` writes the HEAD it
/// reads as the hunt watermark on Done, so a resume that pulled first
/// would mark commits reviewed that no worker ever looked at.
///
/// Proven by removing the remote the sync needs: a cold hunt cannot get
/// past it, and the resume does not care.
#[tokio::test]
async fn a_resumed_hunt_does_not_sync_the_clone() {
    let (dir, path, pool) = fresh_db().await;
    let (repo, _session) = executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let plan = match pick_next(&store, &cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    };
    git(&repo.work, &["remote", "remove", "origin"]);
    let row = store.get_repo_by_id(1).await.unwrap().unwrap();

    let cold = RecordingBackend::new(cfg.work_root.join("out").join("cold.json"), String::new());
    run_hunt(&store, &cfg, &row, &cold, None)
        .await
        .expect_err("a cold hunt has to sync, and cannot without a remote");

    let warm = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );
    let summary = run_hunt(&store, &cfg, &row, &warm, Some(&plan))
        .await
        .expect("a resume must not touch the remote at all");
    assert_eq!(summary.state, Some(JobState::Done));
}

/// A whole cycle picks the suspension up and runs it as the kind that
/// was suspended.
///
/// The tests above call `run_hunt` directly, which assumes the routing
/// they are exercising. This one goes through `run_cycle`, so the
/// suspended row's `kind` really is what decides which executor runs —
/// a resume is not its own job type, it is a hunt that continues.
#[tokio::test]
async fn a_cycle_dispatches_a_resume_to_the_suspended_kinds_executor() {
    let (dir, path, pool) = fresh_db().await;
    executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let backend = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );

    let summary = hunter::scheduler::run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(summary.kind.map(|k| k.to_string()), Some("hunt".to_owned()));
    assert_eq!(summary.state, Some(JobState::Done));
    assert_eq!(summary.ingest.map(|i| i.inserted), Some(1));
    assert!(
        backend.seen().resume_from.is_some(),
        "the cycle must hand the executor the predecessor's transcript"
    );
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "the successor row takes the suspension out of the pool"
    );
}

// -- the cold reservation ----------------------------------------------------

/// A backend that records the reservation the gate asks about and
/// refuses it, so an executor stops at the gate and nothing runs.
#[derive(Default)]
struct GateProbe {
    reserved: OnceLock<i64>,
}

impl GateProbe {
    fn reserved(&self) -> Option<i64> {
        self.reserved.get().copied()
    }
}

#[async_trait::async_trait]
impl Backend for GateProbe {
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        // `set` fails only if one probe gated twice; each run gets its own.
        let _ = self.reserved.set(anticipated_tokens);
        let denied = Verdict::Denied {
            reason: "test: recording the reservation".to_owned(),
            retry_at: None,
        };
        Ok(Outlook {
            normal: denied.clone(),
            prioritized: denied,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn run(
        &self,
        _ws: &Workspace,
        _prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        _resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        anyhow::bail!("every reservation was refused, so nothing may run")
    }
}

/// A cold start reserves the per-kind history, but never less than a
/// cold session's fixed context plus as much again for work: 40,000.
///
/// Live: mis-metered rows once dragged the history to 1,876 tokens, and
/// the gate then started hunts with about 10,000 of headroom that died
/// on their second call. A history above the floor is still what gets
/// reserved — the floor is a minimum, not a replacement.
#[tokio::test]
async fn a_cold_start_reserves_at_least_the_efficiency_floor() {
    let (dir, path, pool) = fresh_db().await;
    cloned_repo(&dir, &pool).await;
    seed_history_at(&pool, 1_876).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let row = store.get_repo_by_id(1).await.unwrap().unwrap();

    let collapsed = GateProbe::default();
    let summary = run_hunt(&store, &cfg, &row, &collapsed, None)
        .await
        .unwrap();
    assert!(summary.denied.is_some(), "the probe refuses: {summary:?}");
    sqlx::query("UPDATE jobs SET tokens_new = 100000 WHERE state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let healthy = GateProbe::default();
    run_hunt(&store, &cfg, &row, &healthy, None).await.unwrap();

    assert_eq!(
        (collapsed.reserved(), healthy.reserved()),
        (Some(40_000), Some(100_000)),
        "max(history, 15_000 + 15_000): the floor over a collapsed history, \
         the history over the floor"
    );
}

/// The reservation `/api/summary` asked the backend about when it
/// previewed the next candidate.
async fn previewed_reservation(store: &Arc<Store>, root: &Path) -> Option<i64> {
    let probe = Arc::new(GateProbe::default());
    let mut config = test_config(root);
    config.ui_dir = root.join("ui");
    std::fs::create_dir_all(&config.ui_dir).unwrap();
    let state = AppState {
        store: Arc::clone(store),
        config: Arc::new(config),
        backend: probe.clone(),
        repo_notes: Arc::new(tokio::sync::Mutex::new(())),
        scheduler: SchedulerHandle {
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            wake: Arc::new(tokio::sync::Notify::new()),
        },
    };
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    probe.reserved()
}

/// The summary's "what's next" preview asks the budget about the same
/// reservation the gate will: a resume's own, and a cold start's
/// floored estimate.
///
/// Anything else previews a budget decision the scheduler is not going
/// to make — "allowed" for a resume the gate will refuse, or for a hunt
/// the floor holds back.
#[tokio::test]
async fn the_summary_preview_reserves_what_the_gate_reserves() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 40_000).await;
    let store = Arc::new(rw_store(&path).await);

    let resume = previewed_reservation(&store, dir.path()).await;

    sqlx::query("DELETE FROM jobs WHERE id = 10")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET tokens_new = 1876 WHERE state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let cold = previewed_reservation(&store, dir.path()).await;

    assert_eq!(
        (resume, cold),
        (Some(CTX + CTX), Some(40_000)),
        "the resume's ctx {CTX} + as much work as context; a cold hunt's floor over 1_876"
    );
}

// -- a suspension is not a failure -------------------------------------------

/// A worker stopped at the cap with a transcript: what the harness
/// reports as a suspension.
fn suspended_at_cap(session: &Path) -> RunResult {
    RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(session.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

/// A real clone registered as repo 1 with one bug finding in `status`,
/// and stub playbooks that render with the slots each builder supplies.
async fn repo_with_finding(
    dir: &TempDir,
    pool: &SqlitePool,
    store: &Store,
    status: FindingStatus,
) -> i64 {
    let repo = GitRepo::with_branch(dir, "feature");
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(repo.work.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("fix.md"), "fix {{WORKTREE}}\n").unwrap();
    std::fs::write(
        playbooks.join("recheck.md"),
        "recheck {{REPO_PATH}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    std::fs::write(playbooks.join("harvest.md"), "harvest {{WORKTREE}}\n").unwrap();
    let (fid, _) = store
        .upsert_finding(
            1,
            &FindingInsert {
                fingerprint: "alpha:README.md:seed:1".to_owned(),
                file: "README.md".to_owned(),
                severity: "medium".to_owned(),
                confidence: 0.9,
                summary: "a real bug".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_status(fid, status).await.unwrap();
    fid
}

/// Three suspensions of one fix in a row leave the finding queued with no
/// failure streak. Counting them rejected a healthy fix as "stuck" after
/// three budget pauses.
#[tokio::test]
async fn repeated_suspensions_of_a_fix_are_not_a_stuck_streak() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Queued).await;
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let suspending = ScriptedBackend::new(|tree| {
        suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_fix(&store, &cfg, &finding, &suspending, None)
            .await
            .unwrap();
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        outcomes.push(summary.outcome);
    }

    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Queued, "{outcomes:?}");
    assert_eq!(finding.fix_attempts, 0);
    assert!(outcomes.iter().all(|o| o.as_deref() == Some("suspended")));
}

/// Three suspensions of one recheck in a row leave the finding rechecking
/// with no failure streak. Counting them reset a healthy recheck back to
/// the inbox as "stuck" after three budget pauses.
#[tokio::test]
async fn repeated_suspensions_of_a_recheck_are_not_a_stuck_streak() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let suspending = ScriptedBackend::new(|tree| {
        suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_recheck(&store, &cfg, &finding, &suspending, None)
            .await
            .unwrap();
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        outcomes.push(summary.outcome);
    }

    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Rechecking, "{outcomes:?}");
    assert_eq!(finding.recheck_attempts, 0);
    assert!(outcomes.iter().all(|o| o.as_deref() == Some("suspended")));
}

/// Minimal `gh pr view --json` payload that `view_pr_engage` can parse.
const PR_VIEW_JSON: &str = r#"{"state":"MERGED","mergeable":"MERGEABLE","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"feature","headRefOid":"deadbeef"}"#;

/// Three suspensions of one harvest in a row leave the PR unharvested with
/// no failure streak. Counting them gave up on a healthy harvest after
/// three budget pauses, marking the PR harvested unreviewed.
#[tokio::test]
async fn repeated_suspensions_of_a_harvest_are_not_a_stuck_streak() {
    let bins = FakeBins::acquire("resume-harvest-streak");
    bins.ok("gh", PR_VIEW_JSON);
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::PrOpen).await;
    // `gh` is handed the repo URL, so it has to be one the forge parses;
    // the clone still fetches from its own origin.
    sqlx::query("UPDATE repos SET url = 'https://github.com/acme/widget' WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, synced_at) \
         VALUES (?1, 7, 'MERGED', 1)",
    )
    .bind(fid)
    .execute(&pool)
    .await
    .unwrap();
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let suspending = ScriptedBackend::new(|tree| {
        suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_harvest(&store, &cfg, &finding, &suspending, None)
            .await
            .unwrap();
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        outcomes.push(summary.outcome);
    }

    let ps = store.get_pr_state(fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None, "still pending harvest: {outcomes:?}");
    assert_eq!(ps.harvest_attempts, 0);
    assert!(outcomes.iter().all(|o| o.as_deref() == Some("suspended")));
}
