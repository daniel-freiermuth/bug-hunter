#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Round-2 write-path + `SpendLedger` behavior tests, run against a writable
//! copy of dev.db (schema-complete, zero rows). Each test seeds its own
//! fixture copy through a plain sqlx pool, then exercises the read-write
//! Store behind the POST handlers via `Store::connect`, the binary's
//! migrating opener.
//!
//! Deviation note (documented in store.rs too): `append_repo_note` stamps
//! dates/times in UTC, while the Python store used `datetime.now()` (local
//! time). Notes are informational free text, never parsed — accepted.

mod support;

use std::path::{Path, PathBuf};

use hunter::backend::SpendLedger;
use hunter::domain::{ForgeName, JobState, RepoJobKind};
use hunter::store::{RepoUpdate, ResumeChainStats, Store, StoreWriteError};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use support::TempDir;

/// Copy dev.db (WAL, checkpointed, no rows) into a scratch directory and
/// open a WRITABLE pool on the copy for fixture inserts and verification
/// reads.
///
/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives the
/// seed pool and the read-write `Store` opened on `path` — which holds its
/// own SQLite pool over that file — and is removed only once both have let
/// it go. Binding it after the `Store`, or discarding it as `_`, deletes
/// the database out from under whoever is still using it. Removing the
/// files by hand at the end of the test body was worse: a failing
/// assertion unwinds straight past it, leaking the copy plus its
/// `-wal`/`-shm` sidecars into a tmpfs that other tests share.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("store-write");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

/// Read-write Store on the same file (the round-2 serve path). The seed
/// pool stays open for verification queries — WAL + 5 s busy timeout make
/// the two connections safe for this sequential test flow.
async fn rw_store(path: &Path) -> Store {
    Store::connect(path).await.unwrap()
}

async fn seed_repo_and_findings(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', 'main', 1, 1000);

        INSERT INTO findings
            (id, type, repo_id, fingerprint, severity, confidence, summary, status,
             created_at, updated_at, verdict_reason, budget_override)
        VALUES
            (1, 'bug', 1, 'fp1', 'low',    0.9, 'one',   'new',    1000, 1000, 'orig', 'once'),
            (2, 'bug', 1, 'fp2', 'medium', 0.8, 'two',   'queued', 1000, 1000, NULL,   'exempt'),
            (3, 'bug', 1, 'fp3', 'high',   0.7, 'three', 'new',    1000, 1000, NULL,   NULL),
            (4, 'bug', 1, 'fp4', 'low',    0.6, 'four',  'new',    1000, 1000, NULL,   NULL);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

// -- 1. set_status --------------------------------------------------------

#[tokio::test]
async fn set_status_updates_row_and_preserves_reason_when_none() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo_and_findings(&pool).await;
    let store = rw_store(&path).await;

    // reason None: status + updated_at change, stored verdict_reason kept.
    store
        .set_finding_status(1, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Queued);
    assert!(f.updated_at > 1000, "updated_at must be bumped to now_ms");
    assert_eq!(f.verdict_reason.as_deref(), Some("orig"));

    // reason Some: verdict_reason replaced.
    store
        .set_finding_verdict(
            1,
            hunter::domain::FindingStatus::Rejected,
            "duplicate of #9",
        )
        .await
        .unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Rejected);
    assert_eq!(f.verdict_reason.as_deref(), Some("duplicate of #9"));

    // Other rows untouched.
    let other = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(other.status, hunter::domain::FindingStatus::New);
    assert_eq!(other.updated_at, 1000);
}

// -- 2. budget overrides --------------------------------------------------

#[tokio::test]
async fn budget_override_set_clear_and_clear_all_only_touches_non_null() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo_and_findings(&pool).await;
    let store = rw_store(&path).await;

    // Set on a clean row, then clear it.
    store.set_budget_override(3, Some("once")).await.unwrap();
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.budget_override.as_deref(), Some("once"));
    assert!(f.updated_at > 1000);

    store.set_budget_override(3, None).await.unwrap();
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.budget_override, None);

    // clear_all: findings 1 ('once') and 2 ('exempt') are the only rows
    // with a non-NULL override left; 3 (just cleared) and 4 must not be
    // touched — 4's updated_at pins the WHERE clause.
    let cleared = store.clear_all_overrides().await.unwrap();
    assert_eq!(cleared, 2);
    for id in [1, 2] {
        let f = store.get_finding(id).await.unwrap().unwrap();
        assert_eq!(f.budget_override, None);
        assert!(f.updated_at > 1000);
    }
    let untouched = store.get_finding(4).await.unwrap().unwrap();
    assert_eq!(
        untouched.updated_at, 1000,
        "NULL-override row must not be updated"
    );

    // Nothing left to clear.
    assert_eq!(store.clear_all_overrides().await.unwrap(), 0);
}

// -- 3. delete_repo -------------------------------------------------------

#[tokio::test]
async fn delete_repo_refusal_message_is_byte_exact_and_clean_delete_works() {
    let (_dir, path, pool) = fresh_db().await;
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'u1', '/tmp/a', 'github', 'main', 1, 1000),
               (2, 'beta',  'u2', '/tmp/b', 'github', 'main', 1, 1000);

        INSERT INTO findings
            (id, type, repo_id, fingerprint, severity, confidence, summary, status, created_at, updated_at)
        VALUES (1, 'bug', 1, 'fp1', 'low', 0.9, 'one', 'new', 1000, 1000),
               (2, 'bug', 1, 'fp2', 'low', 0.9, 'two', 'new', 1000, 1000);

        INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at)
        VALUES (1, 'hunt', 1, 'done', 1000, 1100);
        ",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;

    let err = store.soft_delete_repo(1).await.unwrap_err();
    match err {
        StoreWriteError::Refused(msg) => assert_eq!(
            msg,
            "repo 1 has 2 finding(s) and 1 job(s) -- cannot delete without \
             losing history; pause it instead"
        ),
        StoreWriteError::Db(e) => panic!("expected Refused, got Db({e:?})"),
    }
    // Refusal deleted nothing.
    assert!(store.get_repo_by_id(1).await.unwrap().is_some());

    // Clean repo deletes; the guarded one survives. The flagged row is
    // gone from every read path, but still holds its id until reaped.
    store.soft_delete_repo(2).await.unwrap();
    assert!(store.get_repo_by_id(2).await.unwrap().is_none());
    assert!(store.get_repo_by_id(1).await.unwrap().is_some());
    assert_eq!(
        store.deleted_repo_ids().await.unwrap(),
        vec![2],
        "an invisible repo is still holding its id until its files are gone"
    );
    store.forget_deleted_repo(2).await.unwrap();
    assert!(store.deleted_repo_ids().await.unwrap().is_empty());
}

// -- 3b. create_job vs a soft-deleted repo --------------------------------

/// The scheduler picks a live repo, an operator deletes it mid-cycle, and
/// the runner reaches the job insert afterwards. The foreign key is no
/// defence here — the flagged row is still physically there — so the
/// insert has to check `deleted_at` itself, or the job it writes pins the
/// repo in the half-deleted state forever: `forget_deleted_repo` is a
/// plain DELETE and the FK refuses it while any job references the row.
#[tokio::test]
async fn create_job_refuses_a_soft_deleted_repo_and_leaves_it_reapable() {
    let (_dir, path, pool) = fresh_db().await;
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'u1', '/tmp/a', 'github', 'main', 1, 1000),
               (2, 'beta',  'u2', '/tmp/b', 'github', 'main', 1, 1000);
        ",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;

    // The scheduler's view of repo 2, taken while it was still live.
    let picked = store.get_repo_by_id(2).await.unwrap().unwrap();
    store.soft_delete_repo(2).await.unwrap();

    let err = store
        .create_job(
            RepoJobKind::Hunt.into(),
            picked.id,
            None,
            Some(100_000),
            JobState::Running,
            None,
            None,
        )
        .await
        .unwrap_err();
    match err {
        StoreWriteError::Refused(msg) => {
            assert_eq!(msg, "repo 2 is deleted -- cannot start a hunt job");
        }
        StoreWriteError::Db(e) => panic!("expected Refused, got Db({e:?})"),
    }
    assert!(
        store.list_jobs(10).await.unwrap().is_empty(),
        "a refused insert must write no job row"
    );

    // The guard is keyed on deleted_at, not on the repo being absent: a
    // live repo still gets its job.
    let job = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(100_000),
            JobState::Running,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(job > 0);

    // And the deletion can still finish: no job row is holding repo 2's
    // id, so the reaper's DELETE goes through instead of hitting the FK.
    assert_eq!(store.deleted_repo_ids().await.unwrap(), vec![2]);
    store.forget_deleted_repo(2).await.unwrap();
    assert!(store.deleted_repo_ids().await.unwrap().is_empty());
    let still_there = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM repos WHERE id = 2")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_there, 0, "the reaped row must be gone for good");
}

// -- 4. add_repo / update_repo --------------------------------------------

#[tokio::test]
async fn add_repo_defaults_and_update_repo_partial_fields() {
    let (_dir, path, _pool) = fresh_db().await;
    let store = rw_store(&path).await;

    let id = store
        .add_repo(
            "gamma",
            "https://example.com/g.git",
            std::path::Path::new("/tmp/wr/repos"),
            "main",
            ForgeName::Github,
        )
        .await
        .unwrap();
    let r = store.get_repo_by_id(id).await.unwrap().unwrap();
    assert_eq!(r.name, "gamma");
    assert_eq!(r.url, "https://example.com/g.git");
    assert_eq!(r.path, format!("/tmp/wr/repos/repo-{id}"));
    assert_eq!(r.forge, ForgeName::Github);
    assert_eq!(r.default_branch, "main");
    assert_eq!(r.enabled, 1, "enabled defaults to 1 (schema default)");
    assert!(r.added_at > 0);
    assert_eq!(r.last_hunt_at, None);

    // Partial update: only enabled — every other column untouched.
    store
        .update_repo(
            id,
            &RepoUpdate {
                enabled: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let r = store.get_repo_by_id(id).await.unwrap().unwrap();
    assert_eq!(r.enabled, 0);
    assert_eq!(r.url, "https://example.com/g.git");
    assert_eq!(r.forge, ForgeName::Github);

    // Two fields at once; default_branch stays.
    store
        .update_repo(
            id,
            &RepoUpdate {
                url: Some("git@gitlab.com:x/g.git".to_owned()),
                forge: Some(ForgeName::Gitlab),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let r = store.get_repo_by_id(id).await.unwrap().unwrap();
    assert_eq!(r.url, "git@gitlab.com:x/g.git");
    assert_eq!(r.forge, ForgeName::Gitlab);
    assert_eq!(r.default_branch, "main");
    assert_eq!(r.enabled, 0);

    // All-None is a no-op, not an SQL error.
    store.update_repo(id, &RepoUpdate::default()).await.unwrap();
}

// -- helper for ledger job fixtures ----------------------------------------

async fn seed_repo(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'u', '/tmp/a', 'github', 'main', 1, 1000);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_job(
    pool: &SqlitePool,
    state: &str,
    cap_tokens: Option<i64>,
    tokens_new: Option<i64>,
    finished_at: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO jobs (kind, repo_id, state, cap_tokens, tokens_new, started_at, finished_at) \
         VALUES ('hunt', 1, ?1, ?2, ?3, 1, ?4)",
    )
    .bind(state)
    .bind(cap_tokens)
    .bind(tokens_new)
    .bind(finished_at)
    .execute(pool)
    .await
    .unwrap();
}

// -- 5. running_estimate ----------------------------------------------------

/// Every job here predates `estimated_tokens`, so the sum is entirely
/// the `cap_tokens` fallback — what this asserts is the `state` filter
/// and that a row with neither number does not poison the sum.
#[tokio::test]
async fn running_estimate_counts_running_jobs_only() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_job(&pool, "running", Some(100_000), None, None).await;
    insert_job(&pool, "running", Some(50_000), None, None).await;
    insert_job(&pool, "running", None, None, None).await; // NULL cap: no contribution
    insert_job(&pool, "done", Some(999_999), Some(1_000), Some(2_000)).await;
    let store = rw_store(&path).await;

    assert_eq!(store.running_estimate().await.unwrap(), 150_000);
}

async fn insert_running_job(pool: &SqlitePool, cap_tokens: Option<i64>, estimate: Option<i64>) {
    sqlx::query(
        "INSERT INTO jobs (kind, repo_id, state, cap_tokens, estimated_tokens, started_at) \
         VALUES ('hunt', 1, 'running', ?1, ?2, 1)",
    )
    .bind(cap_tokens)
    .bind(estimate)
    .execute(pool)
    .await
    .unwrap();
}

/// The inflight reservation is what the ramp set aside for the job, not
/// the kill threshold it was granted. The two are independent numbers —
/// the cap will stop bounding spend at all once headroom is the only
/// bound — so a sum over caps under-reserves for a job that is running.
#[tokio::test]
async fn running_estimate_counts_a_running_job_by_its_estimate_not_its_cap() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_running_job(&pool, Some(20_000), Some(204_000)).await;
    let store = rw_store(&path).await;

    assert_eq!(store.running_estimate().await.unwrap(), 204_000);
}

/// Rows written before the column existed have no estimate to recover,
/// so they keep contributing their cap — which is what the reservation
/// meant for them when they were written. A daemon restarted mid-job
/// across the migration would otherwise reserve nothing for it.
#[tokio::test]
async fn running_estimate_falls_back_to_cap_for_rows_without_an_estimate() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_running_job(&pool, Some(50_000), None).await; // pre-migration row
    insert_running_job(&pool, Some(20_000), Some(204_000)).await;
    let store = rw_store(&path).await;

    assert_eq!(store.running_estimate().await.unwrap(), 254_000);
}

// -- 6. finished_since ------------------------------------------------------

#[tokio::test]
async fn finished_since_is_strictly_after_and_skips_denied_null_tokens() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_job(&pool, "done", None, Some(111), Some(10_000)).await; // == ts: excluded
    insert_job(&pool, "done", None, Some(222), Some(10_001)).await; // strictly after
    insert_job(&pool, "denied", None, None, Some(10_500)).await; // NULL tokens_new
    insert_job(&pool, "running", Some(5_000), None, None).await; // running excluded
    insert_job(&pool, "queued", None, None, None).await; // NULL finished_at
    let store = rw_store(&path).await;

    assert_eq!(store.finished_since(10_000).await.unwrap(), 222);
    // Empty range still COALESCEs to 0.
    assert_eq!(store.finished_since(99_999).await.unwrap(), 0);
}

// -- 7. finished_between ----------------------------------------------------

#[tokio::test]
async fn finished_between_is_half_open_start_excluded_end_included() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_job(&pool, "done", None, Some(5), Some(1_000)).await; // == start: excluded
    insert_job(&pool, "done", None, Some(7), Some(1_500)).await; // inside
    insert_job(&pool, "done", None, Some(11), Some(2_000)).await; // == end: included
    insert_job(&pool, "done", None, Some(13), Some(2_001)).await; // past end
    let store = rw_store(&path).await;

    assert_eq!(store.finished_between(1_000, 2_000).await.unwrap(), 18);
}

// -- 8. window_log: log + last observation -----------------------------------

#[allow(clippy::items_after_statements)]
#[tokio::test]
async fn last_window_observation_groups_by_resets_at_and_newest_wins() {
    let (_dir, path, pool) = fresh_db().await;
    const R: i64 = 9_999_999;
    let (a, b, c) = (R + 4_000, R - 4_000, R + 9_000);
    sqlx::query!(
        "INSERT INTO window_log \
         (observed_at, limit_id, used_fraction, status, resets_at, source_age_s) \
         VALUES (100, 'anthropic:5h', 0.1, 'ok', ?, 1), \
                (200, 'anthropic:5h', 0.2, 'ok', ?, 1), \
                (300, 'anthropic:5h', 0.9, 'ok', ?, 1), \
                (250, 'anthropic:7d', 0.7, 'ok', ?, 1)",
        a,
        b,
        c,
        R,
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;

    assert_eq!(
        store
            .last_window_observation("anthropic:5h", R)
            .await
            .unwrap(),
        Some((200, 0.2)),
        "newest row inside resets_at ± 5 s wins; other lids/cycles ignored"
    );

    // A newer row with NULL used_fraction poisons the lookup -> None
    // (even though older rows in the group have values).
    sqlx::query!(
        "INSERT INTO window_log \
         (observed_at, limit_id, used_fraction, status, resets_at, source_age_s) \
         VALUES (400, 'anthropic:5h', NULL, 'ok', ?, 1)",
        R
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        store
            .last_window_observation("anthropic:5h", R)
            .await
            .unwrap(),
        None
    );

    // No matching cycle at all -> None.
    assert_eq!(
        store
            .last_window_observation("anthropic:5h", R + 60_000)
            .await
            .unwrap(),
        None
    );

    // log_window_observation: observed_at = now, source_age_s truncated
    // (int(5.9) == 5), nullable fields stored verbatim.
    store
        .log_window_observation("anthropic:5h", Some(0.3), Some("ok"), Some(42), 5.9)
        .await
        .unwrap();
    let row = sqlx::query_as::<_, (i64, Option<f64>, Option<i64>)>(
        "SELECT observed_at, used_fraction, source_age_s FROM window_log \
         WHERE limit_id = 'anthropic:5h' AND resets_at = 42",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(row.0 > 0);
    assert_eq!(row.1, Some(0.3));
    assert_eq!(row.2, Some(5), "source_age_s is truncated, not rounded");
}

// -- 9. estimate_capacity ----------------------------------------------------

#[allow(clippy::items_after_statements)]
#[tokio::test]
async fn estimate_capacity_returns_max_spend_per_completed_cycle() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    const PERIOD_5H: i64 = 18_000_000;
    // Bucket-aligned so the +3 s duplicate lands in the same 10 s bucket.
    let r1 = (now / 10_000) * 10_000 - 100_000; // completed cycle 1 (newest)
    let r2 = r1 - PERIOD_5H; // completed cycle 2

    let r1d = r1 + 3_000; // same cycle observed twice: 10 s bucket dedups it
    let (o1, o1d, o2) = (r1 - 100, r1 - 50, r2 - 100);
    sqlx::query!(
        "INSERT INTO window_log \
         (observed_at, limit_id, used_fraction, status, resets_at, source_age_s) \
         VALUES (?, 'anthropic:5h', 0.9, 'ok', ?, 1), \
                (?, 'anthropic:5h', 0.95, 'ok', ?, 1), \
                (?, 'anthropic:5h', 0.8, 'ok', ?, 1)",
        o1,
        r1,
        o1d,
        r1d,
        o2,
        r2,
    )
    .execute(&pool)
    .await
    .unwrap();

    // Spend inside cycle 1's window (r1 - period, r1]:
    insert_job(&pool, "done", None, Some(500_000), Some(r1 - 1_000)).await;
    // Spend inside cycle 2's window:
    insert_job(&pool, "done", None, Some(2_000_000), Some(r2 - 1_000)).await;
    // Excluded states inside cycle 1 must not inflate the estimate.
    insert_job(&pool, "denied", None, None, Some(r1 - 500)).await;
    insert_job(&pool, "running", Some(9_999_999), None, None).await;

    let store = rw_store(&path).await;

    // Unknown limit_id (per-model-class) -> None regardless of data.
    assert_eq!(
        store
            .estimate_capacity("anthropic:7d:model-class")
            .await
            .unwrap(),
        None
    );
    // Max across the two completed cycles.
    assert_eq!(
        store.estimate_capacity("anthropic:5h").await.unwrap(),
        Some(2_000_000.0)
    );
    // Different lid: no window_log cycles for 7d -> None (zero spend).
    assert_eq!(store.estimate_capacity("anthropic:7d").await.unwrap(), None);
}

#[tokio::test]
async fn estimate_capacity_zero_spend_returns_none() {
    let (_dir, path, pool) = fresh_db().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // One completed cycle, but no jobs at all.
    let (o, r) = (now - 200_000, now - 100_000);
    sqlx::query!(
        "INSERT INTO window_log \
         (observed_at, limit_id, used_fraction, status, resets_at, source_age_s) \
         VALUES (?, 'anthropic:5h', 0.5, 'ok', ?, 1)",
        o,
        r
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;

    assert_eq!(store.estimate_capacity("anthropic:5h").await.unwrap(), None);
}

// -- append_repo_note (UTC deviation documented at file top) -----------------

#[test]
fn append_repo_note_header_category_and_entry_format() {
    let dir = TempDir::new("notes");
    let work_root: &Path = dir.path();

    // First write creates dir + header.
    let notes = Store::append_repo_note(work_root, 7, "alpha", "first note", None).unwrap();
    let lines: Vec<&str> = notes.lines().collect();
    assert_eq!(lines[0], "# Notes: alpha");
    assert_eq!(lines[1], "");
    let updated = lines[2].strip_prefix("Last updated: ").unwrap();
    assert_eq!(updated.len(), 10, "YYYY-MM-DD (UTC — Python wrote local)");
    assert_eq!(updated.as_bytes()[4], b'-');
    assert_eq!(updated.as_bytes()[7], b'-');
    assert_eq!(lines[3], "");
    // Entry: "- [YYYY-MM-DD HH:MM] note"
    assert!(lines[4].starts_with("- ["), "entry line: {}", lines[4]);
    assert!(lines[4].ends_with("] first note"));
    assert_eq!(lines[4].as_bytes()[3 + 10], b' ', "date/time separator");
    assert_eq!(notes.matches("Last updated:").count(), 1);
    assert!(notes.ends_with("\n\n"), "entries separated by a blank line");

    // Second write appends (no second header), category becomes a heading.
    let notes =
        Store::append_repo_note(work_root, 7, "alpha", "categorized", Some("perf")).unwrap();
    assert_eq!(notes.matches("Last updated:").count(), 1);
    assert!(notes.contains("\n## perf\n- ["));
    assert!(notes.contains("] categorized\n\n"));

    // Bounded re-read is what the POST handler returns. Through
    // `notes_path`, not a hand-built path: notes moved out of the clone
    // once already, and a literal here would have to be found again.
    let on_disk = std::fs::read_to_string(Store::notes_path(work_root, 7)).unwrap();
    assert_eq!(notes, on_disk);
}

/// A brand-new install: `Store::connect` on a path that does not exist yet
/// must create and fully migrate the database.
///
/// Every other test here starts from a copy of `dev.db`, which `build.rs`
/// produces by replaying the migrations through `sqlite3` — so the *sqlx*
/// migrator running against an empty file was never exercised. It was also
/// broken: migration 001 sets `journal_mode = WAL`, sqlx runs migrations in
/// a transaction, and SQLite refuses the change there. Existing databases
/// were already WAL, which made the pragma a silent no-op and hid it.
#[tokio::test]
async fn connect_creates_and_migrates_a_brand_new_database() {
    let dir = TempDir::new("store-bootstrap");
    let path = dir.join("hunter.db");
    assert!(!path.exists(), "precondition: nothing at the path yet");

    let store = Store::connect(&path)
        .await
        .expect("bootstrap a new database");

    // Schema is usable, not merely present.
    let repo_id = store
        .add_repo(
            "acme/widget",
            "git@github.com:acme/widget.git",
            std::path::Path::new("/tmp/wr/repos"),
            "main",
            ForgeName::Github,
        )
        .await
        .expect("insert into the freshly created schema");
    assert!(store.get_repo_by_id(repo_id).await.unwrap().is_some());

    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    // Compare against the directory rather than a hardcoded list, so
    // adding a migration cannot silently stop being covered here.
    let mut on_disk: Vec<i64> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                name.split('_').next()?.parse::<i64>().ok()
            })
            .collect();
    on_disk.sort_unstable();
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, on_disk, "every migration on disk ran");

    // 003 rebuilt findings to drop the legacy single-column UNIQUE; if the
    // rebuild had been skipped, two findings could not share a fingerprint.
    let legacy: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='index' AND name='findings_fingerprint'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap()
    .flatten();
    assert!(
        legacy.is_none(),
        "migration 003 applied on a fresh database"
    );
}

// -- 10. sync_pr_open atomicity -------------------------------------------

fn sync_pr_data(
    fingerprint: &str,
    reason: &str,
    since: i64,
    clear_addressed: bool,
) -> hunter::store::SyncPrData {
    hunter::store::SyncPrData {
        pr_number: 7,
        state: "OPEN".into(),
        mergeable: "MERGEABLE".into(),
        checks: Some("2 pass".into()),
        head_ref: "fix/one".into(),
        head_sha: "deadbee".into(),
        last_activity_at: since,
        last_engaged_activity_at: 0,
        needs_attention: Some(reason.into()),
        attention_fingerprint: Some(fingerprint.into()),
        synced_at: since,
        attention_since: Some(Some(since)),
        clear_addressed,
    }
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct PrStateRow {
    needs_attention: Option<String>,
    attention_fingerprint: Option<String>,
    attention_since: Option<i64>,
    addressed_fingerprint: Option<String>,
    synced_at: Option<i64>,
}

async fn pr_state_row(pool: &SqlitePool) -> PrStateRow {
    sqlx::query_as::<_, PrStateRow>(
        "SELECT needs_attention, attention_fingerprint, attention_since, \
         addressed_fingerprint, synced_at FROM pr_state WHERE finding_id = 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `sync_pr_open` decides its two conditional UPDATEs from the state its
/// UPSERT writes, so a failure between them is not a lost update but a
/// permanently wrong one: the next sync compares against the already
/// committed `attention_fingerprint`, sees no change, and never stamps
/// `attention_since` again.
///
/// Provoking the real trigger (`SQLITE_BUSY` past the 5 s busy timeout) is
/// not something a test can schedule, so the failure is injected where it
/// would land: a trigger that ABORTs the `attention_since` UPDATE. ABORT
/// unwinds only that statement, leaving the caller's transaction (if any)
/// to decide the fate of the UPSERT — which is exactly the question.
#[tokio::test]
async fn sync_pr_open_rolls_back_the_upsert_when_a_later_statement_fails() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo_and_findings(&pool).await;
    let store = rw_store(&path).await;

    store
        .sync_pr_open(
            1,
            &sync_pr_data("fp-old", "review_changes_requested", 1000, false),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE pr_state SET addressed_fingerprint = 'af-old' WHERE finding_id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let before = pr_state_row(&pool).await;

    sqlx::query(
        "CREATE TRIGGER no_attention_since BEFORE UPDATE OF attention_since ON pr_state \
         BEGIN SELECT RAISE(ABORT, 'injected attention_since failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let err = store
        .sync_pr_open(1, &sync_pr_data("fp-new", "merge_conflict", 5000, true))
        .await
        .expect_err("the injected failure must surface to the caller");
    assert!(
        err.to_string().contains("injected attention_since failure"),
        "failed on the injected trigger, not something else: {err}"
    );

    // The whole sync is gone, not just the statement that failed.
    assert_eq!(
        pr_state_row(&pool).await,
        before,
        "the UPSERT must not survive a failure in a statement that depends on it"
    );

    // With the injected failure removed the same sync applies all three
    // writes, so the assertion above measures atomicity, not inertness.
    sqlx::query("DROP TRIGGER no_attention_since")
        .execute(&pool)
        .await
        .unwrap();
    store
        .sync_pr_open(1, &sync_pr_data("fp-new", "merge_conflict", 5000, true))
        .await
        .unwrap();
    assert_eq!(
        pr_state_row(&pool).await,
        PrStateRow {
            needs_attention: Some("merge_conflict".into()),
            attention_fingerprint: Some("fp-new".into()),
            attention_since: Some(5000),
            addressed_fingerprint: None,
            synced_at: Some(5000),
        }
    );
}

// -- 11. resume chains ------------------------------------------------------

async fn seed_second_repo(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (2, 'beta', 'u', '/tmp/b', 'github', 'main', 1, 1000);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_job_with_session(
    pool: &SqlitePool,
    id: i64,
    repo_id: i64,
    state: &str,
    session_file: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, session_file, started_at, finished_at) \
         VALUES (?1, 'hunt', ?2, ?3, ?4, 1, 2)",
    )
    .bind(id)
    .bind(repo_id)
    .bind(state)
    .bind(session_file)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_suspended_job(
    pool: &SqlitePool,
    id: i64,
    repo_id: i64,
    session_file: Option<&str>,
) {
    insert_job_with_session(pool, id, repo_id, "suspended", session_file).await;
}

async fn set_tokens(pool: &SqlitePool, job_id: i64, tokens: i64) {
    sqlx::query("UPDATE jobs SET tokens_new = ?1 WHERE id = ?2")
        .bind(tokens)
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
}

/// Only `suspended` is resumable, and the freshest suspension comes
/// first. A wallclock kill sits in the same table looking similar and
/// must never appear here: an unbounded overrun is the runaway
/// signature, and resuming it would only repeat it.
#[tokio::test]
async fn resumable_jobs_are_the_suspended_ones_newest_first() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_suspended_job(&pool, 10, 1, Some("/s/10/session.jsonl")).await;
    insert_suspended_job(&pool, 11, 1, Some("/s/11/session.jsonl")).await;
    // Both left a session behind, so only `state` can keep them out.
    insert_job_with_session(&pool, 12, 1, "killed", Some("/s/12/session.jsonl")).await;
    insert_job_with_session(&pool, 13, 1, "done", Some("/s/13/session.jsonl")).await;
    let store = rw_store(&path).await;

    let ids: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();

    assert_eq!(ids, vec![11, 10]);
}

/// A suspended job whose repo has been soft-deleted is not a candidate.
/// The clone behind it is being reaped and `create_job` refuses that repo
/// outright, so offering it would hand the scheduler a pick it cannot
/// act on.
///
/// The flag is set by the real `soft_delete_repo`, so the row is in
/// exactly the shape the daemon produces. The job is written directly
/// afterwards because both halves of the write path refuse this pairing
/// — which is the point: this filter is the read-side half of a
/// guarantee `create_job` enforces on the write side, and it is what
/// keeps the candidate list agreeing with it when a delete lands between
/// the scheduler reading the list and acting on it.
#[tokio::test]
async fn resumable_jobs_skip_a_soft_deleted_repo() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_second_repo(&pool).await;
    let store = rw_store(&path).await;
    store.soft_delete_repo(2).await.unwrap();
    insert_suspended_job(&pool, 10, 1, Some("/s/10/session.jsonl")).await;
    insert_suspended_job(&pool, 11, 2, Some("/s/11/session.jsonl")).await;
    let store = rw_store(&path).await;

    let ids: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();

    assert_eq!(
        ids,
        vec![10],
        "the deleted repo's suspension must not be offered"
    );
}

/// Resuming means handing omp one exact session path. A suspended job
/// with no path has nothing to continue, and a resume that falls back to
/// "the newest session for this cwd" is the behaviour that once
/// re-cached an unrelated 290-call transcript for 508,709 tokens on a
/// single call.
#[tokio::test]
async fn resumable_jobs_skip_a_suspension_with_no_session_file() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_suspended_job(&pool, 10, 1, Some("/s/10/session.jsonl")).await;
    insert_suspended_job(&pool, 11, 1, None).await;
    let store = rw_store(&path).await;

    let ids: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();

    assert_eq!(
        ids,
        vec![10],
        "a suspension with no session path cannot be resumed"
    );
}

/// A suspension that something already continues is not offered again.
///
/// The successor row IS the record that this work has been picked up,
/// which is why no `resumed` state exists — a state flag beside the
/// link would be a second copy of the same fact, free to disagree with
/// it. Without this filter the scheduler would resume one transcript
/// every cycle forever, each attempt paying to re-cache the same
/// context: the loop this feature exists to end, rebuilt one level up.
///
/// The successor is left `running` on purpose. That is the state it has
/// for the whole time the filter matters — while the resumed attempt is
/// in flight and the scheduler is picking the next candidate.
#[tokio::test]
async fn resumable_jobs_skip_a_suspension_that_already_has_a_successor() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    insert_suspended_job(&pool, 10, 1, Some("/s/10/session.jsonl")).await;
    insert_suspended_job(&pool, 11, 1, Some("/s/11/session.jsonl")).await;
    let store = rw_store(&path).await;
    store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            None,
            JobState::Running,
            Some(250_000),
            Some(11),
        )
        .await
        .unwrap();

    let ids: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();

    assert_eq!(
        ids,
        vec![10],
        "job 11 is already being continued and must not be handed out twice"
    );
}

/// Three attempts at one piece of work cost what all three cost, are
/// counted as three, and report the biggest of the three — and the
/// answer is the same whichever link you ask from: the scheduler holds
/// the newest, a UI would hold whichever row it rendered.
///
/// All three numbers matter to the caller. The give-up ceiling retires
/// on attempt COUNT as well as on spend, and it sets the largest single
/// attempt aside before judging the rest, so a stats struct that got any
/// one of them wrong would retire the wrong chains silently.
#[tokio::test]
async fn resume_chain_stats_measure_every_attempt_from_any_link() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    let mut chain = Vec::new();
    let mut previous = None;
    for tokens in [120_000, 90_000, 30_000] {
        let id = store
            .create_job(
                RepoJobKind::Hunt.into(),
                1,
                None,
                None,
                JobState::Suspended,
                None,
                previous,
            )
            .await
            .unwrap();
        set_tokens(&pool, id, tokens).await;
        chain.push(id);
        previous = Some(id);
    }

    for link in &chain {
        assert_eq!(
            store.resume_chain_stats(*link).await.unwrap(),
            ResumeChainStats {
                total: 240_000,
                attempts: 3,
                max_single: 120_000,
            },
            "asked from job {link}"
        );
    }

    // Unrelated work is not swept in: the walk follows the link, not the repo.
    let loner = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            None,
            JobState::Done,
            None,
            None,
        )
        .await
        .unwrap();
    set_tokens(&pool, loner, 7_000).await;
    assert_eq!(
        store.resume_chain_stats(loner).await.unwrap(),
        ResumeChainStats {
            total: 7_000,
            attempts: 1,
            max_single: 7_000,
        }
    );
}

/// A cyclic link makes the walk terminate anyway.
///
/// The write path cannot produce this — `resumed_from` is set once at
/// INSERT, naming a row that already exists — so the cycle is written
/// here the only way one can arise in the wild: by hand, as a repaired
/// or partially restored database would carry it.
///
/// Bounded in time on purpose. "Terminates" is not observable from a
/// query that never returns, and without the depth cap this one does not
/// return at all: `UNION` de-duplicates, but each revisit of a node
/// arrives carrying a larger `depth` and is therefore a new row. The
/// capped walk over three rows finishes immediately; the budget exists
/// only to turn a non-terminating query into a failure this test can
/// report, instead of one nextest kills at its own timeout two minutes
/// later.
#[tokio::test]
async fn resume_chain_stats_terminate_on_a_cyclic_link() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    let mut chain = Vec::new();
    let mut previous = None;
    for tokens in [120_000, 90_000, 30_000] {
        let id = store
            .create_job(
                RepoJobKind::Hunt.into(),
                1,
                None,
                None,
                JobState::Suspended,
                None,
                previous,
            )
            .await
            .unwrap();
        set_tokens(&pool, id, tokens).await;
        chain.push(id);
        previous = Some(id);
    }
    sqlx::query("UPDATE jobs SET resumed_from = ?1 WHERE id = ?2")
        .bind(chain[2])
        .bind(chain[0])
        .execute(&pool)
        .await
        .unwrap();

    let stats = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.resume_chain_stats(chain[0]),
    )
    .await
    .expect("the walk must terminate on a cycle, not run until the harness kills it")
    .unwrap();

    assert_eq!(
        stats,
        ResumeChainStats {
            total: 240_000,
            attempts: 3,
            max_single: 120_000,
        },
        "every attempt counted exactly once"
    );
}
