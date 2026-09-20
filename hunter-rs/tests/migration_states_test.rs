#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Every reachable database state can roll forward to head.
//!
//! The bootstrap test covers state 0 (no file) and the Python parity suite
//! covers state 1. Neither says anything about an installation sitting at
//! state 2 or 3, and "the migration chain works" is a claim about *all* of
//! them: a deployment that has been offline for two releases must be able
//! to catch up, and it must land on exactly the schema a fresh install
//! gets, or the two diverge permanently from that point on.
//!
//! So this is parametric over k: build a database that has applied
//! migrations 1..=k and nothing more, hand it to the real opener, and
//! require that it (a) succeeds, (b) ends at head, (c) ends with the same
//! schema as a fresh install, and (d) still has the rows it started with.
//!
//! (d) is not decoration. Migration 003 rebuilds `findings` by copying it
//! to a new table; a mistake there loses every finding on exactly the
//! upgrade path a fresh install never exercises.
//!
//! Partial states are built using sqlx's *own* migration set — its SQL and
//! its checksums, not a re-derivation — so the resume is the real one. A
//! hand-computed checksum would only prove this test agrees with itself.

use std::path::{Path, PathBuf};

use hunter::store::Store;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Executor, Row, SqlitePool};

fn temp_db(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "hunter-mig-{tag}-{}-{nanos}.db",
        std::process::id()
    ))
}

fn cleanup(p: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut s = p.as_os_str().to_owned();
        s.push(suffix);
        std::fs::remove_file(PathBuf::from(s)).ok();
    }
}

async fn open(path: &Path) -> SqlitePool {
    SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap()
}

/// Tables and indexes, with their definitions — what "the same schema"
/// means. `_sqlx_migrations` is bookkeeping and varies by construction.
async fn schema(pool: &SqlitePool) -> Vec<(String, String)> {
    let rows = sqlx::query(
        "SELECT type || ':' || name AS k, COALESCE(sql, '') AS v FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations' ORDER BY k",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.get::<String, _>("k"),
                r.get::<String, _>("v")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        })
        .collect()
}

/// Apply migrations 1..=k exactly as sqlx would, recording each so the
/// real migrator resumes at k+1 instead of starting over.
async fn seed_state(path: &Path, k: usize) -> SqlitePool {
    let pool = open(path).await;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
             version BIGINT PRIMARY KEY,
             description TEXT NOT NULL,
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
             success BOOLEAN NOT NULL,
             checksum BLOB NOT NULL,
             execution_time BIGINT NOT NULL
         )",
    )
    .await
    .unwrap();

    for m in sqlx::migrate!("./migrations").iter().take(k) {
        sqlx::raw_sql(m.sql.as_str())
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("seeding migration {}: {e}", m.version));
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) VALUES (?1,?2,1,?3,0)",
        )
        .bind(m.version)
        .bind(m.description.as_ref())
        .bind(m.checksum.as_ref())
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// A repo and a finding, using only columns present since 001, so the same
/// seed works at every state.
async fn seed_rows(pool: &SqlitePool, fingerprint: &str) {
    sqlx::query(
        "INSERT INTO repos (name, url, path, forge, default_branch, added_at) \
         VALUES ('widget', 'git@github.com:acme/widget.git', '/tmp/widget', 'github', 'main', 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO findings (type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at) \
         VALUES ('bug', 1, ?1, 'high', 0.9, 'carried across the upgrade', 'new', 1, 1)",
    )
    .bind(fingerprint)
    .execute(pool)
    .await
    .unwrap();
}

fn migration_count() -> usize {
    sqlx::migrate!("./migrations").iter().count()
}

/// The head schema a fresh install produces — the thing every other state
/// has to agree with.
async fn fresh_head_schema() -> Vec<(String, String)> {
    let path = temp_db("head");
    let store = Store::connect(&path).await.expect("fresh install migrates");
    drop(store);
    let pool = open(&path).await;
    let s = schema(&pool).await;
    pool.close().await;
    cleanup(&path);
    s
}

#[tokio::test]
async fn every_partial_state_rolls_forward_to_the_same_head() {
    let head = fresh_head_schema().await;
    let n = migration_count();
    assert!(n >= 4, "expected the deployed chain, found {n} migrations");

    // k = 0 is the empty file; k = n is an installation already at head,
    // which must be a no-op rather than an error.
    for k in 0..=n {
        let path = temp_db(&format!("state{k}"));
        let pool = seed_state(&path, k).await;

        // From state 1 the tables exist, so the upgrade carries real rows.
        if k >= 1 {
            seed_rows(&pool, &format!("fp-state-{k}")).await;
        }
        pool.close().await;

        let store = Store::connect(&path)
            .await
            .unwrap_or_else(|e| panic!("state {k} could not roll forward: {e}"));
        drop(store);

        let pool = open(&path).await;

        let applied: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(applied.len(), n, "state {k} did not reach head");

        assert_eq!(
            schema(&pool).await,
            head,
            "state {k} converged on a different schema"
        );

        if k >= 1 {
            let kept: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM findings WHERE fingerprint = ?1")
                    .bind(format!("fp-state-{k}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(kept, 1, "state {k} lost its findings during the upgrade");
        }

        pool.close().await;
        cleanup(&path);
    }
}

/// Re-opening an up-to-date database changes nothing. Every restart of the
/// daemon runs the migrator, so this is the most frequently executed path
/// of all and the one where a non-idempotent migration would do its damage
/// repeatedly.
#[tokio::test]
async fn reopening_at_head_is_idempotent() {
    let path = temp_db("idem");
    let store = Store::connect(&path).await.unwrap();
    drop(store);
    let pool = open(&path).await;
    seed_rows(&pool, "fp-idem").await;
    let before = schema(&pool).await;
    pool.close().await;

    for _ in 0..3 {
        let store = Store::connect(&path).await.expect("reopen at head");
        drop(store);
    }

    let pool = open(&path).await;
    assert_eq!(schema(&pool).await, before, "reopening altered the schema");
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM findings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 1, "reopening altered the data");
    pool.close().await;
    cleanup(&path);
}
