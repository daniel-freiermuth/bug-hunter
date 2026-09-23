#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Router-level tests: `tower::util::ServiceExt::oneshot` against
//! `hunter::server::router` with a Store over a tempdir copy of dev.db
//! (seeded writable, then reopened read-only — same helper pattern as the
//! store tests, deliberately duplicated rather than shared).

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use hunter::config::Config;
use hunter::server::{AppState, router};
use hunter::store::Store;
use serde_json::{Value, json};
use tower::util::ServiceExt;

mod support;

/// `AppState` plus the scratch directory its files live in.
///
/// Field order is the contract: Rust drops fields in declaration order,
/// so `state` — and with it the `Store`'s open SQLite pool — is gone
/// before `_dir` removes the database out from under it.
///
/// `Deref` keeps every call site writing `&state`, and means the guard
/// can only be dropped by dropping the state it came with.
struct TestState {
    state: AppState,
    _dir: support::TempDir,
}

impl std::ops::Deref for TestState {
    type Target = AppState;

    fn deref(&self) -> &AppState {
        &self.state
    }
}

/// Copy dev.db into `dir`, seed one repo + one finding + one event through
/// a writable pool, then reopen the file strictly read-only.
async fn seeded_store(dir: &Path) -> Store {
    let db = dir.join("hunter.db");
    std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/dev.db"), &db).unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'demo', 'https://example.com/demo.git', '/tmp/demo', 'github', 'main', 1, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at) \
         VALUES (1, 'bug', 1, 'fp-1', 'high', 0.9, 'demo finding', 'new', 1000, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO events (id, at, kind, message, finding_id) \
         VALUES (1, 1000, 'hunt', 'demo event', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    Store::connect_read_only(&db).await.unwrap()
}

async fn test_state() -> TestState {
    let dir = support::TempDir::new("server");
    dir.subdir("ui");
    let store = seeded_store(dir.path()).await;
    let config = Config {
        root: dir.path().to_path_buf(),
        work_root: dir.join("data"),
        db_path: dir.join("hunter.db"),
        serve_port: 0,
        ui_dir: dir.join("ui"),
        // Backend inputs (round 2) — inert under NullBackend.
        omp_bin: "omp".into(),
        stale_after_s: 300.0,
        cache_ttl_s: 3600.0,
        poll_s: 2.0,
        session_grace_s: 120,
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
    TestState {
        state: AppState {
            store: Arc::new(store),
            config: Arc::new(config),
            backend: Arc::new(hunter::backend::NullBackend),
            // Read-only GET tests; no cycle is triggered from here.
            repo_notes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            scheduler: hunter::server::SchedulerHandle {
                running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            },
        },
        _dir: dir,
    }
}

async fn get(state: &AppState, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

fn as_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

#[tokio::test]
async fn summary_has_full_zod_shape() {
    let state = test_state().await;
    let (status, _, body) = get(&state, "/api/summary").await;
    assert_eq!(status, StatusCode::OK);
    let v = as_json(&body);
    let obj = v.as_object().unwrap();
    for key in [
        "backend_status_html",
        "counts",
        "type_counts",
        "repos",
        "last_cycle",
        "cycle_running",
        "current_job",
        "next_candidate",
        "scheduler_state",
        "activity_status",
    ] {
        assert!(obj.contains_key(key), "summary missing key {key}");
    }
    let counts = v["counts"].as_object().unwrap();
    for status_key in [
        "new",
        "rechecking",
        "queued",
        "fixing",
        "pr_open",
        "merged",
        "rejected",
        "wontfix",
        "note",
    ] {
        assert!(
            counts.contains_key(status_key),
            "counts missing {status_key}"
        );
    }
    assert_eq!(counts["new"], 1);
    // Seeded 'new' finding + NullBackend denies -> pick_next finds it,
    // decide returns Denied -> activity_status = paused (round 2).
    let kind = v["activity_status"]["kind"].as_str().unwrap();
    assert!(
        kind == "paused" || kind == "warming_up",
        "unexpected activity_status kind: {kind}"
    );
}

#[tokio::test]
async fn finding_missing_and_unknown_ids_are_404() {
    let state = test_state().await;
    for uri in ["/api/finding?id=999999", "/api/finding"] {
        let (status, _, body) = get(&state, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            as_json(&body),
            json!({ "error": "no such finding" }),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn unknown_api_path_is_404_json_with_no_store() {
    let state = test_state().await;
    let (status, headers, body) = get(&state, "/api/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(as_json(&body), json!({ "error": "not found" }));
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
}

#[tokio::test]
async fn root_serves_index_html() {
    let state = test_state().await;
    let stub = b"<!doctype html><title>hunter-rs test</title>";
    std::fs::write(state.config.ui_dir.join("index.html"), stub).unwrap();
    let (status, headers, body) = get(&state, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    assert_eq!(body, stub);
}

/// An unrecognised, non-empty filter must match NO findings.
///
/// Python built `status = ?` / `type = ?` from the raw query value
/// (store.py:587-594), so a value outside the enum simply matched nothing.
/// Parsing into an `Option` and discarding the parse failure inverts that
/// into *no predicate at all*, and a typo then returns the entire corpus —
/// the widest possible answer to the narrowest possible question.
#[tokio::test]
async fn unknown_filter_values_match_nothing_rather_than_everything() {
    let state = test_state().await;

    let (_, _, all) = get(&state, "/api/findings").await;
    let all: Vec<Value> = serde_json::from_slice(&all).unwrap();
    assert!(!all.is_empty(), "fixture has findings to filter");

    for uri in ["/api/findings?status=bogus", "/api/findings?type=bogus"] {
        let (code, _, body) = get(&state, uri).await;
        assert_eq!(code, StatusCode::OK, "{uri} is a valid request");
        let rows: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(rows.is_empty(), "{uri} returned {} findings", rows.len());
    }

    // A known value still filters, and a blank one is still "no filter"
    // (parse_qs semantics: empty == absent).
    let (_, _, body) = get(&state, "/api/findings?status=new").await;
    let known: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(!known.is_empty(), "a valid status must still match");

    let (_, _, body) = get(&state, "/api/findings?status=").await;
    let blank: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(blank.len(), all.len(), "an empty filter is no filter");
}

/// The Host check covers reads, not only mutations.
///
/// The listener binds 127.0.0.1, so this is not about the network: a page
/// the operator visits can resolve its own hostname to 127.0.0.1 and then
/// speak to this port as same-origin, which CORS does not stop. The
/// attacker's name is still in `Host`, and findings carry file paths, code
/// excerpts and notes from private repositories.
#[tokio::test]
async fn a_foreign_host_header_cannot_read_findings() {
    let state = test_state().await;

    for uri in [
        "/api/findings",
        "/api/summary",
        "/api/events",
        "/api/repo/notes?id=1",
        "/",
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, "rebound.attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "GET {uri} must refuse a foreign Host"
        );
    }

    // Loopback names still work, including the absent header the test
    // helpers and curl-without-Host both produce.
    for host in ["localhost", "localhost:8377", "127.0.0.1", "::1"] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/findings")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "Host {host} must be served"
        );
    }
}

/// The scratch directory does not outlive the test that made it.
///
/// Each state carries a copy of `dev.db`; left behind, a full run of this
/// suite deposits one directory per test in the system temp dir, which on
/// this machine is a tmpfs shared with every build. It filled twice in one
/// day, and a truncated file mid-checkout is how that failure presents —
/// nowhere near the tests that caused it.
#[tokio::test]
async fn the_scratch_dir_is_removed_with_the_state() {
    let root = {
        let state = test_state().await;
        let root = state.config.root.clone();
        assert!(root.join("hunter.db").exists(), "fixture: db was created");
        root
    };
    assert!(
        !root.exists(),
        "{} outlived its test and leaks into the temp dir",
        root.display()
    );
}
