#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Behavior tests for Store, run against a writable copy of dev.db
//! (schema-complete, zero rows). Each test seeds its own fixture copy,
//! closes the writer, then reopens through `Store::connect_read_only` —
//! exercising the same read-only path the server uses.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::{FindingStatus, FindingType};
use hunter::store::{FindingFilter, Store};
use sqlx::{Row, SqlitePool};
use support::TempDir;

/// Copy dev.db (WAL, checkpointed, no rows) into a scratch directory and
/// open a WRITABLE pool on the copy for fixture inserts.
///
/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives the
/// pool and any `Store` later opened on `path`, and is removed only once
/// both have closed the file. Binding it after the `Store`, or discarding
/// it as `_`, deletes the database out from under whoever is still reading
/// it. Removing the files by hand at the end of the test body was worse: a
/// failing assertion unwinds straight past it, leaking the copy plus its
/// `-wal`/`-shm` sidecars into a tmpfs that other tests share.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("store");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
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

#[tokio::test]
async fn status_counts_zero_fills_all_statuses() {
    let (_dir, path, pool) = fresh_db().await;
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
}

#[tokio::test]
async fn list_findings_severity_and_combined_filters() {
    let (_dir, path, pool) = fresh_db().await;
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
}

#[tokio::test]
async fn stats_totals_empty_db_preserves_sum_null_semantics() {
    let (_dir, path, pool) = fresh_db().await;
    // No seeding: zero job rows.
    let store = open_store(pool, &path).await;

    let totals = store.stats_totals().await.unwrap();
    assert_eq!(totals.jobs, 0);
    assert_eq!(totals.total_tokens, None);
    assert_eq!(totals.total_calls, None);
    assert_eq!(totals.total_usage_delta, None);
    assert_eq!(totals.done, None);
    assert_eq!(totals.denied, None);
}

#[tokio::test]
async fn current_job_populates_finding_keys_only_with_a_finding() {
    // Newest running job HAS a finding -> summary/fingerprint populated.
    let (_dir, path, pool) = fresh_db().await;
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

    // Newest running job has NO finding -> both keys None (serialized absent).
    let (_dir2, path2, pool2) = fresh_db().await;
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
}

#[test]
fn repo_notes_missing_short_and_truncated() {
    let dir = TempDir::new("notes");
    let work_root: &Path = dir.path();
    // Built by the production path helper rather than by hand: notes have
    // moved out of the clone once already.
    let notes_file = Store::notes_path(work_root, 1);
    std::fs::create_dir_all(notes_file.parent().unwrap()).unwrap();

    // Missing file -> "".
    assert_eq!(Store::repo_notes(work_root, 2), "");

    // Short file passes through untouched.
    std::fs::write(&notes_file, "short note").unwrap();
    assert_eq!(Store::repo_notes(work_root, 1), "short note");

    // >4000 chars -> prefix + exactly the tail 4000 chars.
    let content = "abcde".repeat(1000); // 5000 chars
    std::fs::write(&notes_file, &content).unwrap();
    let notes = Store::repo_notes(work_root, 1);
    let prefix = "...(older notes truncated)...\n";
    assert!(notes.starts_with(prefix));
    let tail = &notes[prefix.len()..];
    assert_eq!(tail.chars().count(), 4000);
    assert_eq!(tail, &content[1000..]);
}

/// The SQL of `Store::{name}`, read out of `src/store.rs` itself.
///
/// `sqlx::query_scalar!` needs a string literal, so the two ledger
/// queries cannot be lifted into a `const` that both the daemon and a
/// test could use. Copying them into the test instead produces a test
/// of the copy: the previous version of this one planned its own SQL
/// and stayed green while the predicate it exists to protect was
/// deleted from the real query.
///
/// Reading the literal back out of the source is the link that has no
/// copy. Anchoring on `async fn <name>(` and the first raw string after
/// it survives rustfmt, comment rewrites and renamed bindings — the
/// query text itself is inside a raw literal, which no formatter
/// touches. Renaming the function or changing the literal's delimiter
/// does break it, and both panic here by name rather than passing.
fn production_sql(name: &str) -> String {
    const SOURCE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/store.rs"));
    let after_fn = SOURCE
        .split_once(&format!("async fn {name}("))
        .unwrap_or_else(|| panic!("src/store.rs has no `async fn {name}(`"))
        .1;
    let from_literal = after_fn
        .split_once("r#\"")
        .unwrap_or_else(|| panic!("no raw-string SQL after `async fn {name}(`"))
        .1;
    from_literal
        .split_once("\"#")
        .unwrap_or_else(|| panic!("unterminated raw-string SQL in `{name}`"))
        .0
        .to_owned()
}

/// The ledger's window sums must be able to use `jobs_finished_at`.
///
/// The index is partial (`finished_at IS NOT NULL AND tokens_new IS NOT
/// NULL`), and SQLite will only use a partial index when the query's
/// WHERE clause implies the index predicate. `SUM` ignores NULLs, so
/// `AND tokens_new IS NOT NULL` looks like a redundant line that a later
/// cleanup would happily delete — and deleting it silently turns both of
/// these back into full scans of every job the daemon has ever run, on
/// the endpoint the UI polls every five seconds. Nothing else would fail.
///
/// So assert the plan, not the result: these two queries must SEARCH
/// using the index, never SCAN. The queries are the daemon's own, taken
/// from `src/store.rs` by `production_sql`.
#[tokio::test]
async fn ledger_window_sums_use_the_finished_at_index() {
    let (_dir, _path, pool) = fresh_db().await;

    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/a.git', '/tmp/a', 'github', 'main', 1, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // A plan is only meaningful once the planner has rows to reason about;
    // with an empty table SQLite may pick a scan regardless.
    for i in 0..500i64 {
        sqlx::query("INSERT INTO jobs (repo_id, kind, state, finished_at, tokens_new) VALUES (1, 'hunt', 'done', ?1, ?2)")
            .bind(1_000_000 + i * 1_000)
            // Half the rows have no token count, which is what makes the
            // partial index worth having.
            .bind(if i % 2 == 0 { Some(i * 7) } else { None })
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();

    for name in ["finished_since", "finished_between"] {
        let sql = production_sql(name);
        // Bound, not substituted: the planner sees the statement the
        // daemon prepares, and a literal in place of a parameter is a
        // different statement to plan.
        let mut explain = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
        for _ in 0..sql.matches('?').count() {
            explain = explain.bind(1_000_000i64);
        }
        let plan: Vec<String> = explain
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("detail"))
            .collect();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("USING INDEX jobs_finished_at"),
            "{name} should use the partial index, planned as: {plan}\nSQL: {sql}"
        );
    }
}
