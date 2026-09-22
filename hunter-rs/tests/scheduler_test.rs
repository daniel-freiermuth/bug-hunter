#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Scheduler selection tests: `anticipated_tokens` (warm/cold percentile
//! choice) and `pick_next` (priority order + eligibility), plus one
//! router-level summary integration over `NullBackend`. Fixture pattern
//! matches the store tests: copy dev.db into a `support::TempDir`, seed via
//! a writable pool, reopen read-only through `Store::connect_read_only`.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::config::Config;
use hunter::domain::{FindingJobKind, RepoJobKind};
use hunter::scheduler::{anticipated_tokens, pick_next};
use hunter::server::{AppState, router};
use hunter::store::Store;
use hunter::util::now_ms;
use serde_json::Value;
use sqlx::SqlitePool;
use support::TempDir;
use tower::util::ServiceExt;

/// Copy dev.db (schema-complete, zero rows) into a scratch directory and
/// open a WRITABLE pool on the copy for fixture inserts.
///
/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives the
/// pool and the `Store` opened on `path`. Removing the files by hand at the
/// end of the test body leaked the whole set on any failing assertion.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("sched");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

/// Close the writer and reopen the same file read-only through Store.
async fn open_store(pool: SqlitePool, path: &Path) -> Store {
    pool.close().await;
    Store::connect_read_only(path).await.unwrap()
}

fn test_config(cache_ttl_s: f64) -> Config {
    let dir = std::env::temp_dir();
    Config {
        root: dir.clone(),
        work_root: dir.join("data"),
        db_path: dir.join("hunter.db"),
        serve_port: 0,
        ui_dir: dir.join("ui"),
        omp_bin: "omp".to_owned(),
        stale_after_s: 300.0,
        cache_ttl_s,
        poll_s: 2.0,
        session_grace_s: 120,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".to_owned(),
        llm_provider: hunter::backends::omp_scavenge::LlmProvider::Anthropic,
        hunt_cap_tokens: 200_000,
        hunt_max_wall_s: 1800,
        hunt_max_findings: 8,
        hunt_rehunt_days: 90,
        fix_cap_tokens: 150_000,
        fix_max_wall_s: 2700,
        scan_interval_days: 1.0,
        modernization_interval_days: 30,
        standards_interval_days: 30,
    }
}

/// One enabled repo. `path` deliberately nonexistent unless a test says
/// otherwise (a huntable-but-not-cloned repo).
async fn seed_repo(pool: &SqlitePool, id: i64, name: &str, enabled: i64, path: &str) {
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (?1, ?2, 'https://example.com/r.git', ?3, 'github', 'main', ?4, 1000)",
    )
    .bind(id)
    .bind(name)
    .bind(path)
    .bind(enabled)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_finding(pool: &SqlitePool, id: i64, repo_id: i64, status: &str, summary: &str) {
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (?1, 'bug', ?2, ?3, 'high', 0.9, ?4, ?5, 1000, 1000)",
    )
    .bind(id)
    .bind(repo_id)
    .bind(format!("fp-{id}"))
    .bind(summary)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

/// Ten finished hunt jobs on repo 1 with `tokens_new` 100, 200, ..., 1000,
/// all finished in the distant past (cold for any sane TTL).
/// p50 index = int(10 * 0.5) = 5 -> 600; p90 index = int(10 * 0.9) = 9 -> 1000.
async fn seed_history(pool: &SqlitePool) {
    for i in 1..=10_i64 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES (?1, 'hunt', 1, 'done', ?2, 1000, 2000)",
        )
        .bind(i)
        .bind(i * 100)
        .execute(pool)
        .await
        .unwrap();
    }
}

// -- anticipated_tokens -------------------------------------------------------

#[tokio::test]
async fn anticipated_tokens_empty_history_is_zero() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 0);
}

#[tokio::test]
async fn anticipated_tokens_cold_p90() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_history(&pool).await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    // Everything finished at epoch-ms 2000 -> stone cold -> p90.
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 1000);
}

#[tokio::test]
async fn anticipated_tokens_warm_p50() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_history(&pool).await;
    // Recent non-denied hunt on the SAME repo. tokens_new NULL keeps the
    // 10-value history intact (the history query filters IS NOT NULL).
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (11, 'hunt', 1, 'done', ?1, ?1)",
    )
    .bind(now_ms() - 60_000)
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 600);
}

#[tokio::test]
async fn anticipated_tokens_warm_requires_same_repo_and_non_denied() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_repo(&pool, 2, "beta", 1, "/nonexistent/beta").await;
    seed_history(&pool).await;
    let recent = now_ms() - 60_000;
    // Recent hunt on a DIFFERENT repo: does not warm repo 1.
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (11, 'hunt', 2, 'done', ?1, ?1)",
    )
    .bind(recent)
    .execute(&pool)
    .await
    .unwrap();
    // Recent DENIED hunt on repo 1: state != 'denied' excludes it.
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (12, 'hunt', 1, 'denied', ?1, ?1)",
    )
    .bind(recent)
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 1000); // still cold -> p90
}

#[tokio::test]
async fn anticipated_tokens_warm_cold_boundary_via_cache_ttl() {
    // Same fixture, same finished_at (1 minute ago) — only the TTL moves:
    // TTL 3600 s puts the job inside the window (warm -> p50), TTL 1 s
    // puts it outside (cold -> p90).
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_history(&pool).await;
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (11, 'hunt', 1, 'done', ?1, ?1)",
    )
    .bind(now_ms() - 60_000)
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;

    let warm_cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &warm_cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 600, "within TTL -> warm -> p50");

    let cold_cfg = test_config(1.0);
    let got = anticipated_tokens(&store, &cold_cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 1000, "outside TTL -> cold -> p90");
}

// -- pick_next ----------------------------------------------------------------

#[tokio::test]
async fn pick_next_empty_db_is_none() {
    let (_dir, path, pool) = fresh_db().await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    assert!(pick_next(&store, &cfg, None).await.unwrap().is_none());
}

#[tokio::test]
async fn pick_next_disabled_repo_never_selected() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 0, "/nonexistent/alpha").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    assert!(pick_next(&store, &cfg, None).await.unwrap().is_none());
}

#[tokio::test]
async fn pick_next_queued_finding_beats_huntable_repo() {
    let (_dir, path, pool) = fresh_db().await;
    // Never-cloned enabled repo: would be a "hunt" candidate on its own.
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Fix.into());
    assert_eq!(c.target_id(), 5);
    assert_eq!(c.repo_id(), 1);
    assert!(matches!(c, hunter::scheduler::Candidate::Finding { .. }));
    assert_eq!(c.budget_override(), None);
    assert_eq!(c.label(), Some("queued bug"));
}

#[tokio::test]
async fn pick_next_attention_beats_queued_fix_and_orders_by_attention_since() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    // Two attention-flagged pr_open findings; #7 has been waiting longer
    // (attention_since 1000 < 2000) and must win despite the lower id
    // sorting later in list_findings' id-DESC order.
    seed_finding(&pool, 7, 1, "pr_open", "older attention").await;
    seed_finding(&pool, 8, 1, "pr_open", "newer attention").await;
    for (fid, since) in [(7_i64, 1000_i64), (8, 2000)] {
        sqlx::query(
            "INSERT INTO pr_state \
             (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
             VALUES (?1, 1, 'OPEN', 'review_comments', ?2, 5000)",
        )
        .bind(fid)
        .bind(since)
        .execute(&pool)
        .await
        .unwrap();
    }
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Engage.into());
    assert_eq!(c.target_id(), 7, "oldest-outstanding attention first");
    assert!(matches!(c, hunter::scheduler::Candidate::Finding { .. }));
}

#[tokio::test]
async fn pick_next_budget_override_jumps_the_queue() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    // Attention row would normally win over a queued fix ...
    seed_finding(&pool, 7, 1, "pr_open", "flagged pr").await;
    sqlx::query(
        "INSERT INTO pr_state \
         (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
         VALUES (7, 1, 'OPEN', 'review_comments', 1000, 5000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // ... but an overridden queued finding jumps everything un-overridden.
    seed_finding(&pool, 5, 1, "queued", "urgent fix").await;
    sqlx::query("UPDATE findings SET budget_override = 'once' WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Fix.into());
    assert_eq!(c.target_id(), 5);
    assert_eq!(c.budget_override(), Some("once"));
}

// -- summary integration (oneshot router, NullBackend) -------------------------

#[tokio::test]
async fn summary_paused_on_denied_candidate() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    let store = open_store(pool, &path).await;

    let mut config = test_config(3600.0);
    config.ui_dir = dir.subdir("ui");
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        backend: Arc::new(hunter::backend::NullBackend),
        repo_notes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        scheduler: hunter::server::SchedulerHandle {
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
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
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();

    assert!(v["current_job"].is_null());
    assert_eq!(v["cycle_running"], Value::Bool(false));

    // NullBackend always denies -> the fix candidate surfaces as denied.
    let nc = &v["next_candidate"];
    assert_eq!(nc["kind"], "fix");
    assert_eq!(nc["id"], 5);
    assert_eq!(nc["label"], "queued bug");
    assert_eq!(nc["is_finding"], Value::Bool(true));
    assert_eq!(nc["is_prioritized"], Value::Bool(false));
    assert_eq!(nc["budget_state"], "denied");
    assert_eq!(nc["budget_reason"], "no window data -- deny until fresh");
    assert!(nc["budget_retry_at"].is_null());

    assert_eq!(v["activity_status"]["kind"], "paused");
    assert_eq!(v["activity_status"]["candidate"]["id"], 5);
    assert_eq!(
        v["backend_status_html"],
        r#"<div class="scv-note">No window data available</div>"#
    );
}
