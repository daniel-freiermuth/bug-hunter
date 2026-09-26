#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Round-3 scheduler executor + store method tests. Each test seeds its own
//! fresh copy of dev.db (schema-complete, zero rows) through a plain sqlx pool.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::{FindingJobKind, FindingType, RepoJobKind};
use hunter::store::Store;
use hunter::types::RunResult;
use sqlx::SqlitePool;
use support::{TempDir, fresh_pool};

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the pool and any `Store` opened on `path` and SQLite's handles are
/// closed before the files go. Returning only the path would leak the
/// whole directory the moment an assertion panicked.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("sched-exec");
    let (path, pool) = fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

async fn rw_store(path: &Path) -> Store {
    Store::connect(path).await.unwrap()
}

async fn seed_repo(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', 'main', 1, 1000);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_findings(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO findings
            (id, type, repo_id, fingerprint, severity, confidence, summary, status,
             created_at, updated_at, fix_attempts, recheck_attempts)
        VALUES
            (1, 'bug', 1, 'fp1', 'low',    0.9, 'one',   'new',     1000, 1000, 0, 0),
            (2, 'bug', 1, 'fp2', 'medium', 0.8, 'two',   'queued',  1000, 1000, 0, 0),
            (3, 'bug', 1, 'fp3', 'high',   0.7, 'three', 'fixing',  1000, 1000, 0, 0),
            (4, 'bug', 1, 'fp4', 'low',    0.6, 'four',  'queued',  1000, 1000, 0, 0);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

// -- 1. _record_job maps RunResult fields correctly --------------------------

#[tokio::test]
async fn record_job_done_state() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(100_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    let rr = RunResult {
        exit_code: Some(0),
        killed_reason: None,
        tokens_new: 42_000,
        calls: 15,
        session_file: Some("/tmp/sess.jsonl".to_owned()),
        duration_s: 120.5,
        stdout_tail: "all good".to_owned(),
        usage_delta: Some(0.05),
    };
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
    assert_eq!(state, hunter::domain::JobState::Done);

    // Verify written fields
    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Done);
    assert_eq!(j.job.tokens_new, Some(42_000));
    assert_eq!(j.job.calls, Some(15));
    assert_eq!(j.job.exit_code, Some(0));
    assert!(j.job.killed_reason.is_none());
    assert_eq!(j.job.session_file.as_deref(), Some("/tmp/sess.jsonl"));
    assert_eq!(j.job.model.as_deref(), Some("claude-4"));
    assert!(j.job.finished_at.is_some());
    // notes should be None for "done" state
    assert!(j.job.notes.is_none());
}

#[tokio::test]
async fn record_job_failed_state() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job_id = store
        .create_job(
            FindingJobKind::Fix.into(),
            1,
            None,
            Some(100_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    let rr = RunResult {
        exit_code: Some(1),
        killed_reason: None,
        tokens_new: 30_000,
        calls: 10,
        session_file: None,
        duration_s: 60.0,
        stdout_tail: "error: something broke\npanic at line 42".to_owned(),
        usage_delta: None,
    };
    let state = hunter::scheduler::record_job(&store, job_id, &rr, None, None)
        .await
        .unwrap();
    assert_eq!(state, hunter::domain::JobState::Failed);

    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Failed);
    assert_eq!(j.job.exit_code, Some(1));
    // notes should contain stdout_tail (up to 500 chars)
    assert!(j.job.notes.is_some());
    assert!(j.job.notes.as_ref().unwrap().contains("panic at line 42"));
}

/// A cap kill that left NO transcript is still `killed`. The session
/// file is what makes a suspension resumable — there is one exact path
/// to hand omp, or there is nothing to continue — so a cap kill without
/// one must not be parked as suspended work that can never be picked up.
#[tokio::test]
async fn record_job_cap_kill_without_a_session_stays_killed() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(50_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 50_000,
        calls: 20,
        session_file: None,
        duration_s: 300.0,
        stdout_tail: "exceeded cap".to_owned(),
        usage_delta: Some(0.1),
    };
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
    assert_eq!(state, hunter::domain::JobState::Killed);

    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Killed);
    assert_eq!(j.job.killed_reason.as_deref(), Some("cap"));
    assert!(j.job.notes.is_some()); // killed => notes from stdout_tail
}

/// A cap kill that DID leave a transcript is a pause, not a failure.
///
/// The work stopped because the window ran out of headroom, and the
/// reasoning that got it that far is still on disk. Recording it as
/// `killed` is what let one repo run ten consecutive hunts over an
/// identical diff range for 1.48M tokens: a killed job advances no
/// watermark, so the same work was re-selected from scratch every cycle.
#[tokio::test]
async fn record_job_cap_kill_with_a_session_suspends() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(50_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 50_000,
        calls: 20,
        session_file: Some("/tmp/sess.jsonl".to_owned()),
        duration_s: 300.0,
        stdout_tail: "exceeded cap".to_owned(),
        usage_delta: Some(0.1),
    };
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
    assert_eq!(state, hunter::domain::JobState::Suspended);

    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Suspended);
    assert_eq!(
        store.list_resumable_jobs().await.unwrap().len(),
        1,
        "a suspension with a transcript must be offered back to the scheduler"
    );
}

/// A wallclock kill is never a suspension, transcript or not.
///
/// An unbounded overrun is the runaway signature: the job was not short
/// of budget, it was not converging. Resuming it would buy the same
/// non-convergence another full wall-clock window.
#[tokio::test]
async fn record_job_wallclock_kill_stays_killed_even_with_a_session() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(50_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("wallclock".to_owned()),
        tokens_new: 50_000,
        calls: 20,
        session_file: Some("/tmp/sess.jsonl".to_owned()),
        duration_s: 1800.0,
        stdout_tail: "ran out of wall clock".to_owned(),
        usage_delta: Some(0.1),
    };
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
    assert_eq!(state, hunter::domain::JobState::Killed);
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "a runaway must never be offered for resumption"
    );
}

// -- 2. create_job + update_job round-trip -----------------------------------

#[tokio::test]
async fn create_and_update_job_round_trip() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    // Create job
    let job_id = store
        .create_job(
            FindingJobKind::Fix.into(),
            1,
            None,
            Some(150_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    assert!(job_id > 0);

    // Verify initial state
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Running);
    assert_eq!(jobs[0].job.kind, FindingJobKind::Fix.into());
    assert_eq!(jobs[0].job.repo_id, 1);
    assert_eq!(jobs[0].job.cap_tokens, Some(150_000));

    // Update with completion
    store
        .complete_job(
            job_id,
            hunter::domain::JobState::Done,
            80_000,
            25,
            Some(0),
            None,
            Some("/tmp/s.json"),
            Some("completed"),
            Some("claude-4"),
            Some(0.03),
            2000,
        )
        .await
        .unwrap();

    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Done);
    assert_eq!(j.job.tokens_new, Some(80_000));
    assert_eq!(j.job.calls, Some(25));
    assert_eq!(j.job.exit_code, Some(0));
    assert_eq!(j.job.notes.as_deref(), Some("completed"));
    assert_eq!(j.job.model.as_deref(), Some("claude-4"));
    assert_eq!(j.job.finished_at, Some(2000));
}

// -- 3. reconcile_orphaned_jobs ----------------------------------------------

#[tokio::test]
async fn reconcile_orphaned_jobs() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Create a "running" job (orphaned)
    let job_id = store
        .create_job(
            FindingJobKind::Fix.into(),
            1,
            Some(3),
            Some(100_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;

    // Finding #3 is at "fixing" (seeded above), job at "running"
    let (stuck_findings, orphaned_jobs) = store.reconcile_orphaned_jobs().await.unwrap();

    // Finding #3 should have been moved from "fixing" to "queued"
    assert_eq!(stuck_findings.len(), 1);
    assert_eq!(stuck_findings[0].id, 3);
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Queued);

    // Job should have been marked "killed"
    assert_eq!(orphaned_jobs.len(), 1);
    assert_eq!(orphaned_jobs[0].id, job_id);
    let jobs = store.list_jobs(10).await.unwrap();
    let j = jobs.iter().find(|j| j.job.id == job_id).unwrap();
    assert_eq!(j.job.state, hunter::domain::JobState::Killed);
    assert_eq!(j.job.killed_reason.as_deref(), Some("orphaned"));
    assert!(j.job.notes.as_deref().unwrap().contains("reconciled"));
    assert!(j.job.finished_at.is_some());
}

// -- 4. ingest_findings: valid inserted, duplicates skipped, invalid counted -

#[tokio::test]
async fn ingest_findings_valid_dup_invalid() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    // Write a temporary findings JSON
    let tmp = dir.join("findings.json");
    let content = serde_json::json!([
        {
            "fingerprint": "fp-ingest-1",
            "bug_class": "logic",
            "severity": "high",
            "confidence": 0.95,
            "summary": "A real bug",
            "file": "src/main.rs",
        },
        {
            "fingerprint": "fp-ingest-2",
            "bug_class": "boundary",
            "severity": "medium",
            "confidence": 0.5,
            "summary": "Another bug",
        },
        "not an object at all",
        {
            "fingerprint": "fp-ingest-3",
            "bug_class": "INVALID_CLASS",
            "severity": "low",
            "confidence": 0.3,
            "summary": "Bad bug class",
        },
    ]);
    std::fs::write(&tmp, content.to_string()).unwrap();

    let result =
        hunter::ingest::ingest_findings(&store, 1, &tmp, Some(FindingType::Bug), None, None).await;
    assert_eq!(result.inserted, 2);
    assert_eq!(result.duplicates, 0);
    assert_eq!(result.invalid, 2); // string + bad bug_class

    // Ingest again: same entries -> duplicates
    let result2 =
        hunter::ingest::ingest_findings(&store, 1, &tmp, Some(FindingType::Bug), None, None).await;
    assert_eq!(result2.inserted, 0);
    assert_eq!(result2.duplicates, 2);
}

#[tokio::test]
async fn ingest_findings_test_gap_type() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    let tmp = dir.join("test-gaps.json");
    let content = serde_json::json!([
        {
            "fingerprint": "tg-1",
            "severity": "medium",
            "confidence": 0.8,
            "summary": "Missing tests for module A",
            "missing_tests": ["test_a", "test_b"],
            "test_file": "tests/test_a.rs",
        },
        {
            "fingerprint": "tg-2",
            "severity": "low",
            "confidence": 0.5,
            "summary": "Missing tests but wrong shape",
            "missing_tests": "not a list",
            "test_file": "tests/test_b.rs",
        },
    ]);
    std::fs::write(&tmp, content.to_string()).unwrap();

    let result =
        hunter::ingest::ingest_findings(&store, 1, &tmp, Some(FindingType::TestGap), None, None)
            .await;
    assert_eq!(result.inserted, 1);
    assert_eq!(result.invalid, 1); // missing_tests not a list
}

// -- 5. finalize_in_progress ------------------------------------------------

#[tokio::test]
async fn finalize_in_progress_resets_when_unchanged() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Finding #3 is "fixing"
    store
        .finalize_in_progress(
            3,
            hunter::domain::FindingStatus::Fixing,
            hunter::domain::FindingStatus::Queued,
        )
        .await
        .unwrap();
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Queued);

    // If status is different, no-op
    store
        .set_finding_status(2, hunter::domain::FindingStatus::Fixing)
        .await
        .unwrap();
    store
        .set_finding_status(2, hunter::domain::FindingStatus::PrOpen)
        .await
        .unwrap(); // changed away
    store
        .finalize_in_progress(
            2,
            hunter::domain::FindingStatus::Fixing,
            hunter::domain::FindingStatus::Queued,
        )
        .await
        .unwrap();
    let f2 = store.get_finding(2).await.unwrap().unwrap();
    assert_eq!(f2.status, hunter::domain::FindingStatus::PrOpen); // untouched
}

// -- 6. upsert_finding + suppressions + known_active -------------------------

#[tokio::test]
async fn upsert_finding_and_queries() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    let row = hunter::store::FindingInsert {
        fingerprint: "my-fp".into(),
        file: "lib.rs".into(),
        severity: "high".into(),
        confidence: 0.9,
        summary: "test finding".into(),
        bug_class: Some("logic".into()),
        ..Default::default()
    };
    let (id, inserted) = store.upsert_finding(1, &row, "bug", None).await.unwrap();
    assert!(inserted);
    assert!(id > 0);

    // Duplicate -> not inserted
    let (id2, inserted2) = store.upsert_finding(1, &row, "bug", None).await.unwrap();
    assert!(!inserted2);
    assert_eq!(id, id2);

    // The finding is 'new' -> in known_active, not in suppressions
    let active = store.known_active(1, "bug").await.unwrap();
    assert_eq!(active.len(), 1);
    let suppressed = store.suppressions(1, "bug").await.unwrap();
    assert_eq!(suppressed.len(), 0);

    // Mark as rejected -> in suppressions
    store
        .set_finding_verdict(id, hunter::domain::FindingStatus::Rejected, "test")
        .await
        .unwrap();
    let active2 = store.known_active(1, "bug").await.unwrap();
    assert_eq!(active2.len(), 0);
    let suppressed2 = store.suppressions(1, "bug").await.unwrap();
    assert_eq!(suppressed2.len(), 1);
}

// -- 7. record/clear attempt tracking ----------------------------------------

#[tokio::test]
async fn fix_attempt_streak_tracking() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Record same failure 3 times
    store.record_fix_attempt(1, "push failed").await.unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 1);
    assert_eq!(f.last_fix_failure.as_deref(), Some("push failed"));

    store.record_fix_attempt(1, "push failed").await.unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 2);

    // Different failure resets streak
    store
        .record_fix_attempt(1, "PR create failed")
        .await
        .unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 1);
    assert_eq!(f.last_fix_failure.as_deref(), Some("PR create failed"));

    // Clear resets everything
    store.clear_fix_attempts(1).await.unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 0);
    assert!(f.last_fix_failure.is_none());
}
