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

/// The migration chain leaves foreign key enforcement on.
///
/// `foreign_keys` is connection-scoped, sqlx sets it when it opens a
/// connection, and it does not reapply connection options when an idle
/// pooled connection is handed out again. Migration 009 runs outside a
/// transaction — the only way its table rebuild can work — so its
/// `PRAGMA foreign_keys = OFF` genuinely takes effect on whichever
/// connection runs it. Without 010 restoring it, that connection goes
/// back into the pool permissive and its next borrower can delete a repo
/// out from under its jobs, visible only later as a `JOIN repos` quietly
/// dropping rows.
///
/// Pinned on a single-connection pool on purpose. Going through `Store`
/// proves nothing: its pool holds several connections, so the query that
/// follows the migration usually lands on a different, freshly
/// configured one and the assertion passes whether or not the chain
/// restores the pragma. One connection makes the mechanism observable
/// rather than probabilistic. Seeded before 009 so the `OFF` executes.
#[tokio::test]
async fn the_migration_chain_leaves_foreign_keys_enforced() {
    let dir = std::env::temp_dir().join(format!("hunter-fk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fk.db");
    let _ = std::fs::remove_file(&path);

    let seeded = seed_state(&path, 8).await;
    seed_rows(&seeded, "fp-fk").await;
    seeded.close().await;

    // One connection, configured the way Store configures its own, so the
    // migrator and the assertion below are guaranteed to share it.
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        on, 1,
        "the migration chain left foreign key enforcement off"
    );

    // And prove it bites, rather than trusting the pragma readout.
    let err = sqlx::query(
        "INSERT INTO findings (type, repo_id, fingerprint, severity, confidence, \
         summary, status, created_at, updated_at) \
         VALUES ('bug', 99999, 'fp-orphan', 'high', 0.9, 's', 'new', 1, 1)",
    )
    .execute(&pool)
    .await
    .expect_err("a finding pointing at no repo must be refused");
    assert!(
        err.to_string().to_lowercase().contains("foreign key"),
        "expected a foreign key violation, got: {err}"
    );

    pool.close().await;
    std::fs::remove_dir_all(&dir).ok();
}

/// Upgrading rewrites name-derived clone paths to id-derived ones.
///
/// Rows written before clone directories were keyed by id point at
/// `repos/<name>`. Both daemons share one `work_root`, so a database that
/// kept the old paths would have hunter-rs looking in one place and the
/// Python daemon in another, and every repo re-cloning on its next job.
///
/// Python has covered this since the rewrite was ported to it; the Rust
/// side did not, which only surfaced on deleting the rewrite from the
/// migration and watching the whole suite stay green.
///
/// Seeded at state 6 so that rolling forward actually executes 007.
#[tokio::test]
async fn upgrading_rewrites_name_derived_clone_paths() {
    let dir = std::env::temp_dir().join(format!("hunter-paths-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("paths.db");
    let _ = std::fs::remove_file(&path);

    let pool = seed_state(&path, 6).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) VALUES \
         (1, 'widget', 'https://e/w.git', '/wr/repos/widget', 'github', 'main', 1), \
         (2, 'odd', 'https://e/o.git', '/somewhere/else', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let widget = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert_eq!(
        widget.path, "/wr/repos/repo-1",
        "a name-derived path must become id-derived"
    );
    // A path that is not `<dir>/<name>` was put there by hand, and the
    // migration's WHERE guard leaves it alone: guessing at it would be
    // worse than leaving it visible.
    let odd = store.get_repo_by_id(2).await.unwrap().unwrap();
    assert_eq!(
        odd.path, "/somewhere/else",
        "an unrecognised path is left alone"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}
