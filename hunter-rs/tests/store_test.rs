#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Behavior tests for Store, run against a writable copy of dev.db
//! (schema-complete, zero rows). Each test seeds its own fixture copy,
//! closes the writer, then reopens through `Store::connect_read_only` —
//! exercising the same read-only path the server uses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use hunter::domain::{FindingStatus, FindingType};
use hunter::store::{FindingFilter, Store};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_temp_path(stem: &str, ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{stem}-{}-{nanos}-{n}{ext}", std::process::id()))
}

/// Copy dev.db (WAL, checkpointed, no rows) to a unique temp path and open
/// a WRITABLE pool on the copy for fixture inserts.
async fn fresh_db() -> (PathBuf, SqlitePool) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("dev.db");
    let path = unique_temp_path("hunter-store-test", ".db");
    std::fs::copy(&src, &path).unwrap();
    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    (path, pool)
}

/// Standard fixture set: one repo, findings across statuses/types/severities,
/// finished jobs, events, and a `pr_state` row for the `pr_open` finding.
async fn seed(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', 'main', 1, 1000);

        INSERT INTO findings
            (id, type, repo_id, fingerprint, severity, confidence, summary, status, created_at, updated_at)
        VALUES
            (1, 'bug',        1, 'fp-low',  'low',    0.9, 'low bug',    'new',     1000, 1000),
            (2, 'bug',        1, 'fp-med',  'medium', 0.8, 'medium bug', 'new',     1001, 1001),
            (3, 'test_gap',   1, 'fp-high', 'high',   0.7, 'high gap',   'pr_open', 1002, 1002),
            (4, 'dep_update', 1, 'fp-dep',  'medium', 0.6, 'dep bump',   'merged',  1003, 1003);

        INSERT INTO jobs
            (id, kind, repo_id, finding_id, state, tokens_new, calls, usage_delta, started_at, finished_at)
        VALUES
            (1, 'hunt', 1, NULL, 'done', 1500, 3, 0.01, 1000, 1100),
            (2, 'fix',  1, 3,    'done', 2500, 5, 0.02, 1200, 1300);

        INSERT INTO events (id, at, kind, message, job_id, finding_id)
        VALUES
            (1, 1000, 'cycle', 'cycle start', NULL, NULL),
            (2, 1001, 'fix',   'fixing',      2,    3);

        INSERT INTO pr_state (finding_id, pr_number, state, needs_attention)
        VALUES (3, 7, 'OPEN', 'review_comments');
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Close the writer and reopen the same file read-only through Store.
async fn open_store(pool: SqlitePool, path: &Path) -> Store {
    pool.close().await;
    Store::connect_read_only(path).await.unwrap()
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        std::fs::remove_file(PathBuf::from(p)).ok();
    }
}

#[tokio::test]
async fn status_counts_zero_fills_all_statuses() {
    let (path, pool) = fresh_db().await;
    seed(&pool).await;
    let store = open_store(pool, &path).await;

    let counts = store.status_counts().await.unwrap();
    // All 9 enum statuses present even when unobserved.
    assert_eq!(counts.len(), FindingStatus::ALL.len());
    for status in FindingStatus::ALL.map(hunter::domain::FindingStatus::as_str) {
        assert!(counts.contains_key(status), "missing status key {status}");
    }
    assert_eq!(counts["new"], 2);
    assert_eq!(counts["pr_open"], 1);
    assert_eq!(counts["merged"], 1);
    assert_eq!(counts["queued"], 0);
    assert_eq!(counts["rechecking"], 0);
    assert_eq!(counts["fixing"], 0);
    assert_eq!(counts["rejected"], 0);
    assert_eq!(counts["wontfix"], 0);
    assert_eq!(counts["note"], 0);

    cleanup(&path);
}

#[tokio::test]
async fn list_findings_severity_and_combined_filters() {
    let (path, pool) = fresh_db().await;
    seed(&pool).await;
    let store = open_store(pool, &path).await;

    // No filters: everything, id DESC.
    let all = store
        .list_findings(&FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![4, 3, 2, 1]
    );
    assert_eq!(all[3].kind, FindingType::Bug); // `type` column lands in the `kind` field

    // severity=medium (rank 2): excludes low, keeps medium+high, id DESC.
    let medium_up = store
        .list_findings(&FindingFilter {
            min_severity_rank: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        medium_up.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![4, 3, 2]
    );

    // All four filters AND together.
    let combined = store
        .list_findings(&FindingFilter {
            status: Some(hunter::domain::FindingStatus::New),
            repo_id: Some(1),
            kind: Some(FindingType::Bug),
            min_severity_rank: Some(2),
        })
        .await
        .unwrap();
    assert_eq!(combined.len(), 1);
    assert_eq!(combined[0].id, 2);
    assert_eq!(combined[0].severity, hunter::domain::Severity::Medium);

    // The old "bogus status matches nothing" test is gone: FindingFilter.status
    // is now Option<FindingStatus>, so invalid values are a compile error.

    cleanup(&path);
}

#[tokio::test]
async fn stats_totals_empty_db_preserves_sum_null_semantics() {
    let (path, pool) = fresh_db().await;
    // No seeding: zero job rows.
    let store = open_store(pool, &path).await;

    let totals = store.stats_totals().await.unwrap();
    assert_eq!(totals.jobs, 0);
    assert_eq!(totals.total_tokens, None);
    assert_eq!(totals.total_calls, None);
    assert_eq!(totals.total_usage_delta, None);
    assert_eq!(totals.done, None);
    assert_eq!(totals.denied, None);

    cleanup(&path);
}

#[tokio::test]
async fn current_job_populates_finding_keys_only_with_a_finding() {
    // Newest running job HAS a finding -> summary/fingerprint populated.
    let (path, pool) = fresh_db().await;
    seed(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state) \
         VALUES (10, 'engage', 1, 3, 'running')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;
    let job = store.current_job().await.unwrap().unwrap();
    assert_eq!(job.id, 10);
    assert_eq!(job.state, hunter::domain::JobState::Running);
    assert_eq!(job.repo_name, "alpha");
    assert_eq!(job.finding_summary.as_deref(), Some("high gap"));
    assert_eq!(job.finding_fingerprint.as_deref(), Some("fp-high"));
    cleanup(&path);

    // Newest running job has NO finding -> both keys None (serialized absent).
    let (path2, pool2) = fresh_db().await;
    seed(&pool2).await;
    sqlx::raw_sql(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state) \
         VALUES (11, 'hunt', 1, NULL, 'running')",
    )
    .execute(&pool2)
    .await
    .unwrap();
    let store2 = open_store(pool2, &path2).await;
    let job2 = store2.current_job().await.unwrap().unwrap();
    assert_eq!(job2.id, 11);
    assert_eq!(job2.finding_id, None);
    assert_eq!(job2.finding_summary, None);
    assert_eq!(job2.finding_fingerprint, None);
    cleanup(&path2);
}

#[test]
fn repo_notes_missing_short_and_truncated() {
    let work_root = unique_temp_path("hunter-notes-test", "");
    let notes_dir = work_root.join("repos").join("repo-1");
    std::fs::create_dir_all(&notes_dir).unwrap();

    // Missing file -> "".
    assert_eq!(Store::repo_notes(&work_root, 2), "");

    // Short file passes through untouched.
    std::fs::write(notes_dir.join("NOTES.md"), "short note").unwrap();
    assert_eq!(Store::repo_notes(&work_root, 1), "short note");

    // >4000 chars -> prefix + exactly the tail 4000 chars.
    let content = "abcde".repeat(1000); // 5000 chars
    std::fs::write(notes_dir.join("NOTES.md"), &content).unwrap();
    let notes = Store::repo_notes(&work_root, 1);
    let prefix = "...(older notes truncated)...\n";
    assert!(notes.starts_with(prefix));
    let tail = &notes[prefix.len()..];
    assert_eq!(tail.chars().count(), 4000);
    assert_eq!(tail, &content[1000..]);

    std::fs::remove_dir_all(&work_root).ok();
}
