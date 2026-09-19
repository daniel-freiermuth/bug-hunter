#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! POST endpoint tests (API-CONTRACT-WRITES.md): oneshot router with
//! `NullBackend` over a WRITABLE tempdir copy of dev.db, with a scheduler
//! handle standing in for the daemon's loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::config::Config;
use hunter::server::{AppState, router};
use hunter::store::Store;
use serde_json::{Value, json};
use tower::util::ServiceExt;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Fresh scratch dir per test (no tempfile dep; leaked in temp on purpose).
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hunter-rs-post-test-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(dir.join("ui")).unwrap();
    dir
}

/// Copy dev.db into `dir`, seed repos/findings through a writable pool,
/// then reopen WRITABLE (POSTs write). Seed layout:
/// - repo 1 "demo" with findings 1 (new, fp-1), 2 (queued, fp-2),
///   3 (new, fp-3) — delete must refuse;
/// - repo 2 "clean" with no findings/jobs — delete must succeed.
async fn seeded_store(dir: &Path) -> Store {
    let db = dir.join("hunter.db");
    std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/dev.db"), &db).unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) VALUES \
         (1, 'demo', 'https://example.com/demo.git', '/tmp/demo', 'github', 'main', 1, 1000), \
         (2, 'clean', 'https://example.com/clean.git', '/tmp/clean', 'github', 'main', 1, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (id, fp, status) in [
        (1, "fp-1", "new"),
        (2, "fp-2", "queued"),
        (3, "fp-3", "new"),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, created_at, updated_at) \
             VALUES (?, 'bug', 1, ?, 'high', 0.9, 'demo finding', ?, 1000, 1000)",
        )
        .bind(id)
        .bind(fp)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
    Store::connect(&db).await.unwrap()
}

async fn test_state() -> AppState {
    let dir = scratch_dir();
    let store = seeded_store(&dir).await;
    let config = Config {
        root: dir.clone(),
        work_root: dir.join("data"),
        db_path: dir.join("hunter.db"),
        serve_port: 0,
        ui_dir: dir.join("ui"),
        omp_bin: "omp".into(),
        stale_after_s: 300.0,
        cache_ttl_s: 3600.0,
        poll_s: 2.0,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".into(),
        hunt_cap_tokens: 200_000,
        hunt_max_wall_s: 1800,
        hunt_max_findings: 8,
        hunt_rehunt_days: 90,
        fix_cap_tokens: 150_000,
        fix_max_wall_s: 2700,
        scan_interval_days: 1.0,
        modernization_interval_days: 30,
        standards_interval_days: 30,
    };
    AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        backend: Arc::new(hunter::backend::NullBackend),
        scheduler: hunter::server::SchedulerHandle {
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            wake: Arc::new(tokio::sync::Notify::new()),
        },
    }
}

async fn post_raw(
    state: &AppState,
    path: &str,
    ctype: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(ct) = ctype {
        builder = builder.header("content-type", ct);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn post(state: &AppState, path: &str, body: Value) -> (StatusCode, Value) {
    post_raw(state, path, Some("application/json"), &body.to_string()).await
}

const POST_PATHS: [&str; 9] = [
    "/api/verdict",
    "/api/cycle",
    "/api/recheck",
    "/api/unqueue",
    "/api/override",
    "/api/repo",
    "/api/repos",
    "/api/repo/delete",
    "/api/repo/notes",
];

// -- §0.1 Content-Type gate ---------------------------------------------------

#[tokio::test]
async fn content_type_gate_415_on_every_post_path() {
    let state = test_state().await;
    for path in POST_PATHS {
        for ctype in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let (status, body) = post_raw(&state, path, ctype, "{}").await;
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{path} with ctype {ctype:?}"
            );
            assert_eq!(
                body["error"], "Content-Type must be application/json",
                "{path} with ctype {ctype:?}"
            );
        }
    }
    // Prefix match: charset suffix passes the gate (fails later, not 415).
    let (status, _) = post_raw(
        &state,
        "/api/recheck",
        Some("application/json; charset=utf-8"),
        r#"{"id": 1}"#,
    )
    .await;
    assert_ne!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// -- §0.2 body parsing ---------------------------------------------------------

#[tokio::test]
async fn body_parse_errors() {
    let state = test_state().await;
    let (status, body) = post_raw(&state, "/api/verdict", Some("application/json"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "empty body");

    let (status, body) = post_raw(&state, "/api/verdict", Some("application/json"), "{nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid JSON body");

    // ACCEPTED DEVIATION: Python 500s on non-object bodies; we keep the
    // 400 envelope.
    let (status, body) = post_raw(&state, "/api/verdict", Some("application/json"), "[1,2]").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "body must be a JSON object");

    // Booleans are NOT integers (§0.2 quirk decision).
    let (status, body) = post(&state, "/api/recheck", json!({ "id": true })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "id must be an integer");
}

// -- §1 /api/verdict -----------------------------------------------------------

#[tokio::test]
async fn verdict_rejected_with_reason_happy_path() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 1, "status": "rejected", "reason": "dupe of #7" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["finding"]["status"], "rejected");
    assert_eq!(body["finding"]["verdict_reason"], "dupe of #7");

    // Read-back: status + verdict_reason persisted, event row written.
    let finding = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(finding.status, hunter::domain::FindingStatus::Rejected);
    assert_eq!(finding.verdict_reason.as_deref(), Some("dupe of #7"));
    let events = state.store.recent_events(10).await.unwrap();
    let event = events.iter().find(|e| e.kind == "verdict").unwrap();
    assert_eq!(event.message, "finding 1 [fp-1] -> rejected: dupe of #7");
    assert_eq!(event.finding_id, Some(1));
}

#[tokio::test]
async fn verdict_reason_required_exact_message() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 1, "status": "rejected" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "reason required for status 'rejected'");

    // Whitespace-only reason normalizes to None -> same failure.
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 1, "status": "wontfix", "reason": "   " }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "reason required for status 'wontfix'");
}

#[tokio::test]
async fn verdict_unknown_finding_404() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 999, "status": "note" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no finding 999");
}

#[tokio::test]
async fn verdict_bad_status_400() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 1, "status": "bogus" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "status must be one of ['queued', 'rejected', 'wontfix', 'note', 'merged']"
    );
}

// -- §§3,4 /api/recheck, /api/unqueue -------------------------------------------

#[tokio::test]
async fn recheck_happy_path_and_precondition() {
    let state = test_state().await;
    // Finding 2 is 'queued' -> exact precondition message.
    let (status, body) = post(&state, "/api/recheck", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "finding #2 is 'queued', not 'new'");

    // Finding 1 is 'new' -> queued for recheck.
    let (status, body) = post(&state, "/api/recheck", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["queued"], json!(true));
    assert_eq!(body["finding"]["status"], "rechecking");
    let events = state.store.recent_events(10).await.unwrap();
    let event = events.iter().find(|e| e.kind == "recheck").unwrap();
    assert_eq!(event.message, "#1 queued for recheck");
}

#[tokio::test]
async fn unqueue_happy_path_and_precondition() {
    let state = test_state().await;
    // Finding 1 is 'new' -> exact precondition message.
    let (status, body) = post(&state, "/api/unqueue", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "finding #1 is 'new', not 'queued'");

    // Finding 2 is 'queued' -> back to new.
    let (status, body) = post(&state, "/api/unqueue", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["finding"]["status"], "new");
    let events = state.store.recent_events(10).await.unwrap();
    let event = events.iter().find(|e| e.kind == "unqueue").unwrap();
    assert_eq!(event.message, "#2 removed from fix queue");
}

// -- §5 /api/override ------------------------------------------------------------

#[tokio::test]
async fn override_set_once_row_updated_and_wakes_loop() {
    let state = test_state().await;
    let wake = Arc::clone(&state.scheduler.wake);
    let (status, body) = post(&state, "/api/override", json!({ "id": 1, "mode": "once" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["finding"]["budget_override"], "once");
    let finding = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(finding.budget_override.as_deref(), Some("once"));
    // Setting an override must not wait for the loop's next natural wake.
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("setting an override must wake the scheduler loop");
}

#[tokio::test]
async fn override_clear_single() {
    let state = test_state().await;
    let (status, _) = post(
        &state,
        "/api/override",
        json!({ "id": 1, "mode": "exempt" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(&state, "/api/override", json!({ "id": 1, "mode": null })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["finding"]["budget_override"], Value::Null);
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "override" && e.message == "#1 budget override: cleared")
    );
}

#[tokio::test]
async fn override_clear_all_returns_cleared_count() {
    let state = test_state().await;
    post(&state, "/api/override", json!({ "id": 1, "mode": "once" })).await;
    post(
        &state,
        "/api/override",
        json!({ "id": 3, "mode": "exempt" }),
    )
    .await;
    let (status, body) = post(
        &state,
        "/api/override",
        json!({ "id": "all", "mode": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true, "cleared": 2 }));
    let finding = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(finding.budget_override, None);
}

#[tokio::test]
async fn override_bad_id_and_bad_mode_exact_messages() {
    let state = test_state().await;
    // "all" with non-null mode falls through to the int check.
    let (status, body) = post(
        &state,
        "/api/override",
        json!({ "id": "all", "mode": "once" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "id must be an integer (or 'all' with mode=null)"
    );

    let (status, body) = post(
        &state,
        "/api/override",
        json!({ "id": 1, "mode": "forever" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "mode must be 'once', 'exempt', or null");
}

// -- §6 /api/repo ------------------------------------------------------------------

#[tokio::test]
async fn repo_update_enabled_toggle_and_no_valid_fields() {
    let state = test_state().await;
    let (status, body) = post(&state, "/api/repo", json!({ "id": 1, "enabled": false })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["repo"]["enabled"], 0);
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repo" && e.message == "updated demo: enabled=0")
    );

    // Invalid forge is silently skipped -> nothing left to update.
    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": 1, "forge": "bitbucket" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "no valid fields to update");
}

// -- §7 /api/repos -------------------------------------------------------------------

#[tokio::test]
async fn repo_add_happy_then_duplicate_409() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "newrepo", "url": "https://github.com/x/newrepo.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["repo"]["name"], "newrepo");
    assert_eq!(body["repo"]["forge"], "github");
    assert_eq!(body["repo"]["default_branch"], "main");
    assert_eq!(body["repo"]["enabled"], 1);
    let path = body["repo"]["path"].as_str().unwrap();
    assert!(path.ends_with("repos/newrepo"), "path was {path}");

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "newrepo", "url": "https://github.com/x/other.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "repo 'newrepo' already exists");
}

#[tokio::test]
async fn repo_add_bad_name_and_missing_url() {
    let state = test_state().await;
    for name in ["../../etc", "/etc/passwd", "..", "a/../../../b", ""] {
        let (status, body) = post(
            &state,
            "/api/repos",
            json!({ "name": name, "url": "https://github.com/x/y.git" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {name:?}");
        assert_eq!(body["error"], "invalid repo name", "name {name:?}");
    }
    let (status, body) = post(&state, "/api/repos", json!({ "name": "ok_name-1.0" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "name and url are required");
}

#[tokio::test]
async fn repo_add_detects_gitlab_from_url_and_rejects_unknown_forge() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "glrepo", "url": "https://gitlab.com/g/r.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["repo"]["forge"], "gitlab");

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "bb", "url": "https://bitbucket.org/x/y.git", "forge": "bitbucket" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "unknown forge 'bitbucket' (choose from github, gitlab)"
    );
}

// -- §8 /api/repo/delete ----------------------------------------------------------------

#[tokio::test]
async fn repo_delete_refused_with_history_then_success_when_clean() {
    let state = test_state().await;
    // Repo 1 has 3 findings, 0 jobs -> exact store refusal message.
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "repo 1 has 3 finding(s) and 0 job(s) -- cannot delete without losing history; \
         pause it instead"
    );

    // Repo 2 is clean -> deleted, bare {"ok": true} body.
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true }));
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repo" && e.message == "deleted clean (#2)")
    );

    // Gone: a second delete is a 404.
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no repo 2");
}

// -- §9 /api/repo/notes -----------------------------------------------------------------

#[tokio::test]
async fn repo_notes_append_201_and_validations() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repo/notes",
        json!({ "id": 1, "note": "check the flaky retry test", "category": "testing" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["ok"], json!(true));
    let notes = body["notes"].as_str().unwrap();
    assert!(notes.contains("# Notes: demo"), "notes: {notes}");
    assert!(notes.contains("## testing"), "notes: {notes}");
    assert!(
        notes.contains("check the flaky retry test"),
        "notes: {notes}"
    );
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repo" && e.message == "note added to demo [testing]")
    );

    let (status, body) = post(&state, "/api/repo/notes", json!({ "id": 1, "note": "  " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "note must be a non-empty string");

    let (status, body) = post(
        &state,
        "/api/repo/notes",
        json!({ "id": 1, "note": "x", "category": 7 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "category must be a string");

    let (status, body) = post(&state, "/api/repo/notes", json!({ "id": 99, "note": "x" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no repo 99");
}

// -- §2 /api/cycle ------------------------------------------------------------------------

/// Contract §2: an idle scheduler accepts the trigger with 202.
#[tokio::test]
async fn cycle_starts_returns_202() {
    let state = test_state().await;
    let (status, body) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["started"], true);
}

/// Contract §2: the 202 is not cosmetic -- it must actually reach the loop.
/// The handler notifies before responding, so the permit is already held
/// and `notified()` resolves immediately.
#[tokio::test]
async fn cycle_wakes_the_scheduler_loop() {
    let state = test_state().await;
    let wake = Arc::clone(&state.scheduler.wake);
    let (status, _) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("POST /api/cycle must wake the scheduler loop");
}

/// Contract §2: a cycle already under way is refused, not queued twice.
#[tokio::test]
async fn cycle_while_running_is_409_busy() {
    let state = test_state().await;
    state.scheduler.running.store(true, Ordering::SeqCst);
    let (status, body) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "busy");
}
