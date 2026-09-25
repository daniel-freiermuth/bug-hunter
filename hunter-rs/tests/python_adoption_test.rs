#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Adopting a database the Python daemon created.
//!
//! Both daemons share one `work_root` and one `hunter.db`. Python builds its
//! schema from `hunter/schema.sql` plus its own probe-and-ALTER loop, so the
//! file it leaves behind has the full schema and no `_sqlx_migrations`.
//! Handing that to sqlx replays the chain over a schema that already has its
//! effects. Three `ADD COLUMN` migrations collide with a column the Python
//! schema already carries — 004 on `findings.standard_section`, 008 on
//! `repos.deleted_at`, 011 on `findings.found_by_job` — but only 008 is
//! ever observed to fail, because migration 003 rebuilds `findings` from
//! its own column list first and throws `standard_section` and
//! `found_by_job` away, values included, before 004 and 011 get there.
//! Nothing rebuilds `repos`, so 008 is where the daemon dies.
//!
//! The live production database escaped this only by accident of timing: it
//! was adopted before Python grew those columns, so its history exists and
//! the colliding migrations never replay. Every *new* adoption was broken.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use hunter::domain::ForgeName;
use hunter::store::Store;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection, SqlitePool};
use support::{TempDir, db_copy};

/// The interpreter that can import the Python daemon.
///
/// `HUNTER_TEST_PYTHON` first, which is how CI supplies it; then
/// `hunter/.venv`, the venv hunter/README.md has developers create. A
/// developer who has never created that venv gets a skip, because
/// failing a Rust suite over a Python interpreter would be hostile.
///
/// CI gets no such mercy. A skip there is indistinguishable from a pass,
/// and this test is the only thing that exercises adoption against the
/// schema Python actually writes -- so when `CI` is set and no interpreter
/// is configured, that is a broken workflow and it fails loudly.
fn python() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HUNTER_TEST_PYTHON") {
        let p = PathBuf::from(p);
        assert!(
            p.exists(),
            "HUNTER_TEST_PYTHON={} does not exist",
            p.display()
        );
        return Some(p);
    }
    let p = repo_root().join("hunter/.venv/bin/python");
    if p.exists() {
        return Some(p);
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "CI must set HUNTER_TEST_PYTHON: without it the adoption test \
         skips and the job passes having tested nothing"
    );
    None
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hunter-rs has a parent directory")
        .to_path_buf()
}

/// Build a database by running the real Python daemon's `Store`
/// constructor — schema.sql plus its probe-and-ALTER upgrade loop, exactly
/// what a live Python installation leaves on disk.
fn build_python_db(py: &Path, dir: &TempDir) -> PathBuf {
    let script = "\
import sys
sys.path.insert(0, 'hunter')
from pathlib import Path
from hunter.store import Store
from hunter.types import Config
d = Path(sys.argv[1])
Store(Config(work_root=d, db_path=d / 'hunter.db'))
";
    let out = Command::new(py)
        .arg("-c")
        .arg(script)
        .arg(dir.path())
        .current_dir(repo_root())
        .output()
        .expect("spawn the Python daemon");
    assert!(
        out.status.success(),
        "python store bootstrap failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.join("hunter.db")
}

async fn raw_pool(path: &Path) -> SqlitePool {
    SqlitePool::connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .expect("open raw pool")
}

fn migration_count() -> usize {
    sqlx::migrate!("./migrations").iter().count()
}

/// `(version, execution_time)` for every recorded migration.
///
/// `execution_time` is the discriminator that makes these tests sharp: sqlx
/// writes the real duration when it *runs* a migration and the sentinel `-1`
/// when it records one as applied without running it. So the same table
/// says which path a database took, and a test can pin "this database was
/// migrated" separately from "this database was adopted".
async fn history(pool: &SqlitePool) -> Vec<(i64, i64)> {
    sqlx::query("SELECT version, execution_time FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("read migration history")
        .iter()
        .map(|r| (r.get::<i64, _>(0), r.get::<i64, _>(1)))
        .collect()
}

async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("probe sqlite_master")
        > 0
}

/// A database the Python daemon built is adopted, keeps its data, and works.
///
/// The three columns checked by name are the three that collide with a
/// replay: `repos.deleted_at` (migration 008), `findings.found_by_job`
/// (011) and `findings.standard_section` (004).
///
/// Seeding rows *before* adoption is not decoration. Making the ADD COLUMNs
/// idempotent instead of stamping would still be wrong: migration 003
/// rebuilds `findings` from the column list as of 003, silently dropping
/// `standard_section` and `found_by_job` values on the way past. Asserting
/// only that connect succeeds would accept that.
#[tokio::test]
async fn a_python_built_database_is_adopted_with_its_data_intact() {
    let Some(py) = python() else {
        eprintln!(
            "SKIP: no HUNTER_TEST_PYTHON and no hunter/.venv (see hunter/README.md), \
             cannot build a Python database"
        );
        return;
    };
    let dir = TempDir::new("adopt-python");
    let path = build_python_db(&py, &dir);

    let seed = raw_pool(&path).await;
    assert!(
        !table_exists(&seed, "_sqlx_migrations").await,
        "the Python daemon must not leave sqlx history, or this test proves nothing"
    );
    sqlx::query(
        "INSERT INTO repos (name, url, path, forge, default_branch, added_at) \
         VALUES ('widget', 'git@github.com:acme/widget.git', '/w/repo-1', 'github', 'main', 1)",
    )
    .execute(&seed)
    .await
    .expect("seed repo");
    sqlx::query(
        "INSERT INTO findings (type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at, found_by_job, standard_section) \
         VALUES ('bug', 1, 'fp-1', 'high', 0.9, 'from the Python daemon', 'new', 1, 1, 77, 'S3')",
    )
    .execute(&seed)
    .await
    .expect("seed finding");
    seed.close().await;

    let store = Store::connect(&path)
        .await
        .expect("adopt the Python database");

    // Usable through the ordinary API, not just openable.
    let id = store
        .add_repo(
            "gadget",
            "git@github.com:acme/gadget.git",
            Path::new("/w"),
            "main",
            ForgeName::Github,
        )
        .await
        .expect("add a repo after adoption");
    let repo = store
        .get_repo_by_name("gadget")
        .await
        .expect("read back")
        .expect("the repo exists");
    assert_eq!(repo.id, id);
    drop(store);

    let pool = raw_pool(&path).await;
    let found_by_job: Option<i64> =
        sqlx::query_scalar("SELECT found_by_job FROM findings WHERE fingerprint = 'fp-1'")
            .fetch_one(&pool)
            .await
            .expect("findings.found_by_job survives adoption and is queryable");
    assert_eq!(found_by_job, Some(77));
    let section: Option<String> =
        sqlx::query_scalar("SELECT standard_section FROM findings WHERE fingerprint = 'fp-1'")
            .fetch_one(&pool)
            .await
            .expect("findings.standard_section survives adoption");
    assert_eq!(section.as_deref(), Some("S3"));
    let deleted_at: Option<i64> =
        sqlx::query_scalar("SELECT deleted_at FROM repos WHERE name = 'widget'")
            .fetch_one(&pool)
            .await
            .expect("repos.deleted_at survives adoption and is queryable");
    assert_eq!(deleted_at, None);

    let after = history(&pool).await;
    assert_eq!(
        after.len(),
        migration_count(),
        "every embedded migration must be recorded, or the next start replays the rest"
    );
    assert!(
        after.iter().all(|&(_, exec)| exec == -1),
        "adoption records migrations without running them: {after:?}"
    );

    // Restarting the daemon must not stamp a second time.
    let store = Store::connect(&path).await.expect("reopen after adoption");
    drop(store);
    assert_eq!(
        history(&pool).await,
        after,
        "re-opening restamped the history"
    );
    pool.close().await;
}

/// An empty database is migrated, not stamped.
///
/// The adoption path has to be invisible to a fresh install: if it fired
/// here it would record the whole chain as applied against a database that
/// has none of it, and the daemon would start with no tables at all.
///
/// The other half of "adoption stays out of the way" — a database that
/// already has sqlx history keeps upgrading by RUNNING its remaining
/// migrations rather than having them stamped — is pinned by
/// `migration_states_test::every_partial_state_rolls_forward_to_the_same_head`,
/// which refuses every state 1..n the moment adoption stops checking for
/// existing history. A dedicated test here for the already-at-head case was
/// written and deleted: it passed with this whole feature reverted, because
/// sqlx's `Migrator::skip` is itself a no-op on versions already recorded,
/// so nothing it asserted could ever have gone wrong.
#[tokio::test]
async fn an_empty_database_is_migrated_rather_than_stamped() {
    let dir = TempDir::new("adopt-empty");
    let path = dir.join("hunter.db");

    let store = Store::connect(&path).await.expect("fresh install migrates");
    drop(store);

    let pool = raw_pool(&path).await;
    let rows = history(&pool).await;
    assert_eq!(rows.len(), migration_count());
    assert!(
        rows.iter().all(|&(_, exec)| exec >= 0),
        "a fresh install must RUN its migrations, not record them as applied: {rows:?}"
    );
    assert!(table_exists(&pool, "findings").await);
    assert!(table_exists(&pool, "repos").await);
    pool.close().await;
}

/// A database that looks hunter-shaped but is missing part of the schema is
/// refused, by name, instead of being stamped.
///
/// Without the guard this is the worst outcome available: stamping records
/// the migration that would have added the column as done, so it never runs,
/// and a loud failure at startup becomes a failure deep inside a request
/// handler months later. The refusal has to name what is missing, because
/// "cannot be adopted" alone leaves the operator with nothing to act on.
#[tokio::test]
async fn a_hunter_shaped_database_missing_a_column_is_refused() {
    let dir = TempDir::new("adopt-divergent");
    let path = db_copy(&dir, "divergent");

    let pool = raw_pool(&path).await;
    // Head schema, history erased, one column short: exactly the shape that
    // must not be mistaken for a Python-built database.
    sqlx::query("DROP TABLE _sqlx_migrations")
        .execute(&pool)
        .await
        .expect("erase history");
    sqlx::query("ALTER TABLE findings DROP COLUMN standard_section")
        .execute(&pool)
        .await
        .expect("drop a column");
    pool.close().await;

    let Err(err) = Store::connect(&path).await else {
        panic!("a database short of head must not be adopted");
    };
    let err = err.to_string();
    assert!(
        err.contains("findings.standard_section"),
        "the refusal must name the missing column, got: {err}"
    );
    assert!(
        err.contains("cannot be adopted"),
        "the refusal must say what it refused to do, got: {err}"
    );

    let pool = raw_pool(&path).await;
    assert!(
        !table_exists(&pool, "_sqlx_migrations").await,
        "a refused database must be left exactly as it was found"
    );
    pool.close().await;
}

/// A `repos` table without `AUTOINCREMENT` is refused, not stamped.
///
/// It has every column and every unique constraint, so a shape check that
/// looked only at those would adopt it — recording migration 009 as applied
/// without running the rebuild that adds the keyword, after which SQLite
/// hands a freed repo id to the next insert.
///
/// The statement keeps a comment that *mentions* AUTOINCREMENT, as
/// `hunter/schema.sql` does inside `CREATE TABLE repos`. That is the case a
/// substring check gets wrong, and the one this test exists to hold.
#[tokio::test]
async fn a_repos_table_without_autoincrement_is_refused() {
    let dir = TempDir::new("adopt-no-autoinc");
    let path = db_copy(&dir, "no-autoinc");

    // One connection: writable_schema is connection-scoped.
    let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .expect("open");
    sqlx::query("DROP TABLE _sqlx_migrations")
        .execute(&mut conn)
        .await
        .expect("erase history");
    sqlx::query("PRAGMA writable_schema = ON")
        .execute(&mut conn)
        .await
        .expect("writable_schema");
    let changed = sqlx::query(
        "UPDATE sqlite_master
         SET sql = replace(sql, 'PRIMARY KEY AUTOINCREMENT',
                           'PRIMARY KEY -- AUTOINCREMENT, explained but absent
')
         WHERE type = 'table' AND name = 'repos'",
    )
    .execute(&mut conn)
    .await
    .expect("drop the keyword")
    .rows_affected();
    assert_eq!(changed, 1, "fixture must actually rewrite repos");
    sqlx::query("PRAGMA writable_schema = OFF")
        .execute(&mut conn)
        .await
        .expect("writable_schema off");
    conn.close().await.expect("close");

    let Err(err) = Store::connect(&path).await else {
        panic!("a repos table without AUTOINCREMENT must not be adopted");
    };
    let err = err.to_string();
    assert!(
        err.contains("AUTOINCREMENT on repos"),
        "the refusal must name the missing property, got: {err}"
    );

    let pool = raw_pool(&path).await;
    assert!(
        !table_exists(&pool, "_sqlx_migrations").await,
        "a refused database must be left exactly as it was found"
    );
    pool.close().await;
}

/// A database missing a non-unique index is refused, by name.
///
/// Nothing would *fail* without it: stamping records the migration that
/// built the index as done, so it is never built, and every query still
/// answers -- by scanning. That makes it the quietest way adoption can go
/// wrong, which is exactly why it has to be loud here.
#[tokio::test]
async fn a_database_missing_an_index_is_refused() {
    let dir = TempDir::new("adopt-no-index");
    let path = db_copy(&dir, "no-index");

    let pool = raw_pool(&path).await;
    sqlx::query("DROP TABLE _sqlx_migrations")
        .execute(&pool)
        .await
        .expect("erase history");
    sqlx::query("DROP INDEX jobs_finished_at")
        .execute(&pool)
        .await
        .expect("drop an index");
    pool.close().await;

    let Err(err) = Store::connect(&path).await else {
        panic!("a database missing an index must not be adopted");
    };
    let err = err.to_string();
    assert!(
        err.contains("index jobs_finished_at on jobs"),
        "the refusal must name the missing index, got: {err}"
    );

    let pool = raw_pool(&path).await;
    assert!(
        !table_exists(&pool, "_sqlx_migrations").await,
        "a refused database must be left exactly as it was found"
    );
    pool.close().await;
}
