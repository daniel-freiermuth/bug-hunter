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
use std::sync::OnceLock;

use hunter::backend::{Backend, JobClass, Outlook, Verdict};
use hunter::config::Config;
use hunter::domain::{FindingStatus, JobState, RepoJobKind};
use hunter::scheduler::{
    Candidate, ResumePlan, pick_next, record_job, run_fix, run_harvest, run_hunt, run_recheck,
};
use hunter::store::{FindingInsert, Store};
use hunter::types::RunResult;
use sqlx::SqlitePool;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, git};

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

/// Read-write Store: `pick_resume` retires a chain that blew the give-up
/// ceiling, so the selection path under test writes.
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
    for id in 1..=3_i64 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES (?1, 'hunt', 1, 'done', ?2, 1000, 2000)",
        )
        .bind(id)
        .bind(Z)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A worker transcript in omp's session format.
///
/// Two assistant calls. The first is a small opening exchange; the
/// second is where the session had got to when it was suspended, and its
/// `input + cacheRead + cacheWrite` is [`CTX`]. Only that last record
/// should count: re-establishing a session costs the context it had
/// reached, not the sum of every call that built it.
fn seed_session(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"message":{"role":"assistant","usage":"#,
            r#"{"input":500,"output":200,"cacheRead":0,"cacheWrite":37000}}}"#,
            "\n",
            r#"{"message":{"role":"user","content":"noise"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","usage":"#,
            r#"{"input":50000,"output":900,"cacheRead":140000,"cacheWrite":10000}}}"#,
            "\n",
        ),
    )
    .unwrap();
    path
}

/// One suspended hunt on repo 1, with a transcript and a measured cost.
async fn seed_suspension(pool: &SqlitePool, id: i64, session: &Path, tokens: i64) {
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at) \
         VALUES (?1, 'hunt', 1, 'suspended', ?2, 'cap', ?3, 1000, 2000)",
    )
    .bind(id)
    .bind(session.to_string_lossy().to_string())
    .bind(tokens)
    .execute(pool)
    .await
    .unwrap();
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
        Some(Candidate::Resume { plan, .. }) => plan,
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
#[tokio::test]
async fn resume_reserves_context_plus_the_remaining_estimate() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
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
        CTX + (Z - spent),
        "ctx {CTX} + max({Z} - {spent}, 25_000)"
    );
}

/// When the chain has already outspent the per-kind typical, the
/// progress term is floored rather than going negative.
///
/// `z - chain_spent` is meaningless once it is below zero: a negative
/// budget for the work still to do is not a small budget. Floored at
/// enough for the worker to make real progress after re-caching —
/// reserving only the re-cache would fund a resume that can do nothing
/// but re-cache and be killed again, turning every resume into another
/// suspension.
#[tokio::test]
async fn resume_reservation_floors_the_progress_term_when_the_chain_overspent() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Over the typical, but well under the give-up ceiling of 3x.
    let spent = Z + 30_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(
        plan.anticipated,
        CTX + 25_000,
        "z - chain_spent is {} here, so the floor is what binds",
        Z - spent
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
    let missing = dir.path().join("never-written.jsonl");
    let spent = 40_000;
    seed_suspension(&pool, 10, &missing, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());

    assert_eq!(plan.anticipated, Z + (Z - spent));
}

// -- the give-up ceiling -----------------------------------------------------

/// A chain that has cost three times its kind's typical is abandoned,
/// not resumed again.
///
/// Work that expensive and still unfinished is not going to finish, and
/// without a ceiling the chain resumes forever with every link looking
/// individually reasonable. Falling through to normal selection is the
/// other half: the cycle still does something useful.
#[tokio::test]
async fn the_give_up_ceiling_retires_the_chain_and_falls_through() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 3 * Z + 1).await;
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
    let (state, reason, notes) = job_row(&pool, 10).await;
    assert_eq!(state, JobState::Failed.as_str());
    assert_eq!(reason.as_deref(), Some("give-up"));
    assert!(
        notes.unwrap().contains("giving up after 300001 tok"),
        "the record must name what the chain cost"
    );
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "a retired chain must never be offered again"
    );
}

/// Exactly at the ceiling is still resumable — "three times the typical
/// and still not done" is the condition, and a chain that has spent
/// precisely 3x has not passed it.
#[tokio::test]
async fn the_give_up_ceiling_is_exclusive() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 3 * Z).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(pick_next(&store, &cfg, None).await.unwrap());
    assert_eq!(plan.predecessor_id, 10);
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

/// A suspension whose working directory is gone is skipped, not offered.
///
/// A resumed worker continues a conversation, not a filesystem: pointing
/// it at a tree that has since been reclaimed would have it edit files
/// that no longer exist. Skipping at selection rather than refusing from
/// inside the executor is what keeps it from being offered again every
/// cycle — and since this tier outranks rotation, that would starve it.
#[tokio::test]
async fn a_suspension_whose_working_directory_is_gone_is_skipped() {
    let (dir, path, pool) = fresh_db().await;
    let clone = seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    std::fs::remove_dir_all(&clone).unwrap();
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
        "an uncloned repo is a hunt candidate, never a resume, got {picked:?}"
    );
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
          started_at, finished_at) \
         VALUES (20, 'hunt', 2, 'suspended', ?1, 'cap', 10000, 1000, 2000)",
    )
    .bind(session.to_string_lossy().to_string())
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
        .unwrap();
    let plan = ResumePlan {
        kind: RepoJobKind::Hunt.into(),
        repo_id: 1,
        repo: "alpha".to_owned(),
        finding_id: None,
        predecessor_id: 10,
        origin_job_id: 10,
        session_file: session,
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
        cwd: &Path,
        prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        // `set` fails only if this backend ran twice, which no test
        // here does — each one builds its own.
        let _ = self.seen.set(Seen {
            cwd: cwd.to_path_buf(),
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

/// A real clone with a bare origin, registered as repo 1, plus history,
/// a transcript and a suspended hunt whose id is `pred`.
async fn executable_repo(dir: &TempDir, pool: &SqlitePool, pred: i64) -> (GitRepo, PathBuf) {
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
    seed_history(pool).await;
    let session = seed_session(dir);
    seed_suspension(pool, pred, &session, 40_000).await;
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
    let (repo, session) = executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = match pick_next(&store, &cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => plan,
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
    assert_eq!(seen.cwd, repo.work);
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
    assert_eq!(estimated, Some(CTX + (Z - 40_000)));

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
        Some(Candidate::Resume { plan, .. }) => plan,
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
    let session = seed_session(&dir);
    let suspending = ScriptedBackend::new(move |_| suspended_at_cap(&session));

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
    let session = seed_session(&dir);
    let suspending = ScriptedBackend::new(move |_| suspended_at_cap(&session));

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
    let session = seed_session(&dir);
    let suspending = ScriptedBackend::new(move |_| suspended_at_cap(&session));

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
