#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! POST endpoint tests (API-CONTRACT-WRITES.md): oneshot router with
//! `NullBackend` over a WRITABLE tempdir copy of dev.db, with a scheduler
//! handle standing in for the daemon's loop.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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

/// Refuse to run when this process can write through a read-only
/// directory.
///
/// Three tests below block reclamation by taking write permission off
/// `repos/`. Root ignores that bit, so under root the removal succeeds,
/// the row is reaped, and every one of them goes green having exercised
/// the opposite of what it claims to test — the silent kind of pass,
/// where the assertions still run and still hold.
///
/// Checked by trying it rather than by comparing the euid to 0: the euid
/// is a proxy, and `CAP_DAC_OVERRIDE` or a filesystem mounted without
/// permission enforcement defeats the tests the same way while leaving
/// it non-zero. It also keeps this to `std`.
///
/// Runs in its own directory and restores it before asserting, so a
/// caller's `repos/` is untouched whether this passes or panics — which
/// is also why it must be called BEFORE the caller's own chmod.
fn require_enforced_permissions() {
    let probe = support::TempDir::new("perm-probe");
    let dir = probe.path();
    let mut perms = std::fs::metadata(dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(dir, perms).unwrap();
    let wrote = std::fs::write(dir.join("probe"), "x").is_ok();
    let mut perms = std::fs::metadata(dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(dir, perms).unwrap();
    assert!(
        !wrote,
        "this process writes through a read-only directory (running as \
         root?), and this test blocks reclamation with one — it would \
         pass while proving the opposite"
    );
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

async fn test_state() -> TestState {
    let dir = support::TempDir::new("post");
    dir.subdir("ui");
    let store = seeded_store(dir.path()).await;
    let config = Config {
        root: dir.path().to_path_buf(),
        work_root: dir.join("data"),
        db_path: dir.join("hunter.db"),
        serve_port: 0,
        ui_dir: dir.join("ui"),
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
        hunt_max_wall_s: 1800,
        hunt_max_findings: 8,
        hunt_rehunt_days: 90,
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
            repo_notes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            scheduler: hunter::server::SchedulerHandle {
                running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                wake: Arc::new(tokio::sync::Notify::new()),
            },
        },
        _dir: dir,
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

/// Captures `tracing` output for the duration of one request.
///
/// `ApiError::Internal` answers the client with a bare `internal error`
/// envelope on purpose, so the log is the only place the detail exists.
/// A poisoned lock is recovered rather than unwrapped: a panic mid-test
/// must surface as that panic, not as a second one from the log sink.
#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl LogSink {
    fn buf(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf()).into_owned()
    }
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for LogSink {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        self.clone()
    }
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
    // The clone directory is keyed by id, never by the name: a name is a
    // display string and may differ from another only in case, which is the
    // same directory on NTFS and APFS.
    let rid = body["repo"]["id"].as_i64().expect("new repo id");
    let path = body["repo"]["path"].as_str().unwrap();
    assert!(
        path.ends_with(&format!("repos/repo-{rid}")),
        "path was {path}"
    );
    assert!(
        !path.contains("newrepo"),
        "name must not reach the path: {path}"
    );

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

/// Deleting a repo must remove its notes file.
///
/// The original reason was id reuse: SQLite handed a freed rowid to the
/// next INSERT, so a notes file left behind was inherited by the next
/// repo to take that id. Migration 009 made `repos.id` AUTOINCREMENT
/// and closed that premise — see `a_reclaimed_id_is_never_reissued`.
///
/// The removal still has to happen, and since notes moved out of the
/// clone to `notes/repo-{id}.md` it has to be explicit: removing
/// `repo-{id}` no longer takes them with it. `reap_repo` only drops the
/// row once both are gone, so notes that survive keep a deleted repo's
/// private context on disk and its row alive indefinitely.
#[tokio::test]
async fn deleting_a_repo_removes_its_notes() {
    let state = test_state().await;

    // A repo with history cannot be deleted at all, so add a fresh one.
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "throwaway", "url": "https://example.com/throwaway.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let (status, _) = post(
        &state,
        "/api/repo/notes",
        json!({ "id": rid, "note": "context for later" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Via the production path builder: notes have moved once already.
    let notes_path = Store::notes_path(&state.config.work_root, rid);
    assert!(notes_path.exists(), "fixture: notes file was written");

    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !notes_path.exists(),
        "notes survived the delete: the deleted repo's private notes are still on disk"
    );
}

/// ...and when it cannot remove them, it must say so.
///
/// Deletion is two-phase, so a failure here is not the caller's problem:
/// the row is already flagged deleted and invisible to every query, and
/// the reaper owns the retry. The caller gets a 200.
///
/// What the failure must not do is drop the row, because the row is what
/// holds the id while the old repo's files are still sitting at its path.
///
/// The obstruction is `repos/` made read-only: unlinking `repo-{id}`
/// needs write permission on the parent, not on the directory itself.
///
/// The envelope stays generic — `ApiError::Internal` deliberately never
/// leaks internals to a client — so the id and the remediation reach the
/// operator through the log, and that is where they are asserted.
#[tokio::test]
async fn a_repo_whose_files_survive_keeps_its_id_reserved_until_reaped() {
    require_enforced_permissions();
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "throwaway", "url": "https://example.com/throwaway.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    // Make the repo's directory impossible to remove by taking write
    // permission off its parent, which is what the handler needs in order
    // to unlink the entry.
    let repos_root = state.config.work_root.join("repos");
    let repo_dir = repos_root.join(format!("repo-{rid}"));
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("occupied"), "x").unwrap();
    let mut perms = std::fs::metadata(&repos_root).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&repos_root, perms).unwrap();
    let sink = LogSink::default();
    let capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish(),
    );
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    drop(capture);
    // Before any assertion can panic: a directory left unwritable defeats
    // the scratch dir's cleanup and leaks it into /tmp for good.
    let mut perms = std::fs::metadata(&repos_root).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&repos_root, perms).unwrap();
    // The repo *is* deleted — only reclaiming its files failed, and that
    // is not the caller's problem to retry.
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let logged = sink.text();
    assert!(
        logged.contains(&format!("repo {rid} deleted, but reclaiming "))
            && logged.contains("its files stay until the reaper retries"),
        "a failed reclamation must say the id is still held: {logged}"
    );

    // No retry here on purpose: a second delete re-attempts reclamation,
    // which would succeed now that permissions are restored and release
    // the very id this test is about. Retry behaviour has its own test.

    // ...but the row is still holding the id, which is the whole point:
    // handing it to the next INSERT while those files remain is what put
    // one repo's clone under another repo's name.
    assert_eq!(
        state.store.deleted_repo_ids().await.unwrap(),
        vec![rid],
        "an unreclaimed repo must keep its id reserved"
    );

    // The reaper finishes the job once the obstruction is gone.
    let reaped = hunter::server::reap_deleted_repos(
        &state.store,
        &state.config.work_root,
        &state.repo_notes,
    )
    .await;
    assert_eq!(reaped, 1);
    assert!(!repo_dir.exists(), "the reaper must remove the directory");
    assert!(
        state.store.deleted_repo_ids().await.unwrap().is_empty(),
        "and only then release the id"
    );
}

/// The same must hold when it is the *notes* that cannot be removed.
///
/// Reclamation removes two things, and the sibling test above only ever
/// reaches the first: the clone directory fails, so the notes removal is
/// never attempted and neither is the branch that has to propagate its
/// failure. Make notes best-effort — `let _ = remove_file(..)`, which is
/// tempting since they are secondary to the clone — and the row is
/// dropped while the operator's private context is still on disk, with
/// nothing left that will ever retry it. That is the one outcome
/// two-phase deletion exists to prevent, and nothing else fails.
///
/// The obstruction is a directory at the notes path: `unlink` refuses a
/// directory outright, so this needs no permission games and holds under
/// root, where the sibling test has to bow out.
#[tokio::test]
async fn notes_that_cannot_be_removed_keep_the_id_reserved_too() {
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "throwaway", "url": "https://example.com/throwaway.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let notes_path = Store::notes_path(&state.config.work_root, rid);
    std::fs::create_dir_all(&notes_path).unwrap();
    std::fs::write(notes_path.join("occupied"), "x").unwrap();
    // The clone was never made, so its removal reports NotFound and is
    // skipped -- this test only gets to say anything because the notes
    // step runs after it.
    let clone_dir = Store::repo_dir(&state.config.work_root.join("repos"), rid);
    assert!(
        !clone_dir.exists(),
        "fixture: the clone must be absent, or it fails first"
    );

    let sink = LogSink::default();
    let capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish(),
    );
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    drop(capture);
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        state.store.deleted_repo_ids().await.unwrap(),
        vec![rid],
        "notes left on disk must keep the repo reapable, not release its id"
    );
    let logged = sink.text();
    assert!(
        logged.contains(&format!("repo {rid} deleted, but reclaiming ")),
        "a failed reclamation must be logged: {logged}"
    );

    // And once the obstruction is gone the reaper finishes, which is what
    // makes the retained row a retry rather than a wedge.
    std::fs::remove_dir_all(&notes_path).unwrap();
    let reaped = hunter::server::reap_deleted_repos(
        &state.store,
        &state.config.work_root,
        &state.repo_notes,
    )
    .await;
    assert_eq!(reaped, 1);
    assert!(
        state.store.deleted_repo_ids().await.unwrap().is_empty(),
        "and only then release the id"
    );
}

/// A repo URL that would execute when clicked is rejected at the write.
///
/// Defence in depth, not the only guard: `ReposPage` builds an `<a href>`
/// only when `isHttpUrl(repo.url)` accepts the value and renders anything
/// else as text, so such a URL is inert in today's UI. The write gate
/// matters because the UI is not the only reader of `repo.url`, a guard on
/// one page is one refactor from being dropped, and rejecting once at the
/// write is cheaper than escaping every read.
/// Both spellings of an ssh clone URL must keep working: the scp-like
/// `git@host:owner/repo.git`, which carries no scheme at all, and the
/// explicit `ssh://git@host/owner/repo.git`, which is allow-listed.
#[tokio::test]
async fn repo_urls_with_an_executable_scheme_are_rejected() {
    let state = test_state().await;

    for bad in [
        "javascript:alert(document.domain)",
        "JavaScript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "vbscript:msgbox(1)",
    ] {
        let (status, body) =
            post(&state, "/api/repos", json!({ "name": "evil", "url": bad })).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} was accepted: {body}"
        );
    }

    // And the schemes the daemon actually clones with still work.
    for good in [
        "https://github.com/acme/widget.git",
        "http://git.internal/acme/widget.git",
        "git@github.com:acme/widget.git",
        // The rejection message promises ssh; the explicit spelling of an
        // ssh clone URL has to be accepted for that to be true.
        "ssh://git@github.com/acme/widget.git",
    ] {
        let name = format!("ok{}", good.len());
        let (status, body) = post(&state, "/api/repos", json!({ "name": name, "url": good })).await;
        assert_eq!(status, StatusCode::CREATED, "{good} was rejected: {body}");
    }
}

/// The same gate on the update path, which takes a url too.
#[tokio::test]
async fn updating_a_repo_url_is_validated_as_well() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": 1, "url": "javascript:alert(1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    let (status, _) = post(
        &state,
        "/api/repo",
        json!({ "id": 1, "url": "git@github.com:acme/widget.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Deleting a repo removes everything keyed to its id.
///
/// Ids are no longer reused (migration 009 made `repos.id` AUTOINCREMENT),
/// but the directory removal is still what releases the row: `reap_repo`
/// deletes the record only once `repo-{id}` is gone. Leaving the clone
/// behind used to hand it to the next repo on that id, and `sync_repo`
/// skips cloning when the path already exists.
#[tokio::test]
async fn deleting_a_repo_removes_its_directory() {
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "ephemeral", "url": "https://example.com/e.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");
    let repo_dir = std::path::PathBuf::from(body["repo"]["path"].as_str().unwrap());

    // Stand in for a clone and its notes.
    std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
    std::fs::write(repo_dir.join("NOTES.md"), "secret").unwrap();
    std::fs::write(repo_dir.join(".git/config"), "[remote]").unwrap();

    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !repo_dir.exists(),
        "{} survived the delete, so repo {rid}'s row cannot be released and its clone is still on disk",
        repo_dir.display()
    );
}

/// The name comes back immediately; the id never does.
///
/// A name is the operator's to reuse the moment they are told the repo is
/// gone -- blocking it on a filesystem operation they cannot see would be
/// inexplicable. The id is never reissued at all: `repos.id` is
/// AUTOINCREMENT (migration 009), so it cannot land on the old clone the
/// way a recycled rowid once put one repo's code under another's name.
/// What pending reclamation governs is only the name and the clone path.
#[tokio::test]
async fn deleting_frees_the_name_at_once_and_never_reissues_the_id() {
    require_enforced_permissions();
    let state = test_state().await;
    let repos_dir = state.config.work_root.join("repos");

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "recycled", "url": "https://example.com/a.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let first = body["repo"]["id"].as_i64().unwrap();

    // Block reclamation so the flagged row is still there afterwards.
    let blocked = repos_dir.join(format!("repo-{first}"));
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::write(blocked.join("occupied"), "x").unwrap();
    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    let (status, _) = post(&state, "/api/repo/delete", json!({ "id": first })).await;

    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&repos_dir, perms).unwrap();
    assert_eq!(status, StatusCode::OK);

    // Name is free at once, even though the files are not yet reclaimed.
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "recycled", "url": "https://example.com/b.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let second = body["repo"]["id"].as_i64().unwrap();

    // ...and the new repo did NOT get handed the old one's id. Since
    // migration 009 made `repos.id` AUTOINCREMENT that holds whether or
    // not reclamation is still pending -- `a_reclaimed_id_is_never_reissued`
    // proves the reaped case -- so this assertion is about the id never
    // moving backwards, not about the flagged row occupying the rowid.
    // What pending reclamation actually governs here is the name being
    // free again and the clone path below.
    assert_ne!(
        second, first,
        "the id was reissued while the previous repo's clone was still on disk"
    );
    assert_ne!(
        body["repo"]["path"].as_str().unwrap(),
        blocked.to_string_lossy(),
        "the replacement must not be pointed at the old clone"
    );
}

/// One deletion, one event.
///
/// Deletion became two phases, and the log call that belonged to the old
/// single phase was left behind next to the new one, so every delete
/// wrote the same row twice — visible in `GET /api/events` and in the log
/// view. The contract specifies one.
#[tokio::test]
async fn deleting_a_repo_logs_exactly_one_event() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "logged-once", "url": "https://example.com/l.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().unwrap();

    let (status, _) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(status, StatusCode::OK);

    let events = state.store.recent_events(200).await.unwrap();
    let deletions: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains(&format!("deleted logged-once (#{rid})")))
        .collect();
    assert_eq!(
        deletions.len(),
        1,
        "expected a single deletion event, got {deletions:#?}"
    );
}

/// A retry lands as success while the deletion is still pending.
///
/// Soft deletion made the row invisible to `get_repo_by_id`, so a client
/// that lost the first response and retried was told the repo never
/// existed — for a request that had in fact succeeded.
///
/// The window is bounded by reclamation, deliberately. Once the
/// directory is gone the row is gone too, and that id is then
/// indistinguishable from one that was never a repo; answering 200
/// forever would mean keeping a tombstone per deleted repo to buy
/// nothing but a status code. So: 200 while pending, 404 after, and 404
/// for an id that never existed.
#[tokio::test]
async fn a_retried_delete_is_a_success_while_reclamation_is_pending() {
    require_enforced_permissions();
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "twice", "url": "https://example.com/t.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().unwrap();

    // Block reclamation so the row stays flagged and the deletion is
    // genuinely still pending when the retry arrives.
    let repos_dir = state.config.work_root.join("repos");
    let dir = repos_dir.join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("occupied"), "x").unwrap();
    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    let (first, _) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    let (retry, retry_body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;

    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    assert_eq!(first, StatusCode::OK);
    assert_eq!(retry, StatusCode::OK, "a retry must not 404: {retry_body}");

    // Once reclaimed, the id is ordinary again and answers 404.
    hunter::server::reap_deleted_repos(&state.store, &state.config.work_root, &state.repo_notes)
        .await;
    let (after, _) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(after, StatusCode::NOT_FOUND);

    let (unknown, body) = post(&state, "/api/repo/delete", json!({ "id": 987_654 })).await;
    assert_eq!(
        unknown,
        StatusCode::NOT_FOUND,
        "an id that was never a repo is still a 404: {body}"
    );
}

/// A fully reclaimed id is never handed out again.
///
/// Every repo-lifecycle hazard on this branch descended from id reuse: a
/// replacement inheriting a deleted repo's clone, its notes, its cached
/// notes in the browser. Two-phase deletion closes the window where
/// *files* outlive the row, but not the one where a *client* does — a
/// browser tab holding the old id can pause, delete or annotate whatever
/// repo now answers to it, and that window is as long as the tab is
/// open. AUTOINCREMENT removes the premise: `sqlite_sequence` only moves
/// forward, so a stale id can only ever miss.
#[tokio::test]
async fn a_reclaimed_id_is_never_reissued() {
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "first", "url": "https://example.com/1.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let first = body["repo"]["id"].as_i64().unwrap();

    // Delete it and let reclamation finish, so the row is entirely gone —
    // the state in which a rowid alias would be reissued.
    let (status, _) = post(&state, "/api/repo/delete", json!({ "id": first })).await;
    assert_eq!(status, StatusCode::OK);
    hunter::server::reap_deleted_repos(&state.store, &state.config.work_root, &state.repo_notes)
        .await;
    assert!(state.store.deleted_repo_ids().await.unwrap().is_empty());

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "second", "url": "https://example.com/2.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let second = body["repo"]["id"].as_i64().unwrap();

    assert!(
        second > first,
        "id {first} was reissued as {second}; a client still holding {first} \
         would now be addressing a different repository"
    );
}

/// Changing a repo's URL must not strand its clone.
///
/// `sync_repo` refuses to work in a directory whose origin is not the
/// repo's URL, which is how it recognises somebody else's checkout at
/// the path. Leaving the clone on the old origin turned that check into
/// a permanent block: every job for the repo failed until the directory
/// was removed by hand.
#[tokio::test]
async fn changing_the_url_repoints_the_existing_clone() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "movable", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    // A real clone carrying the old origin.
    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/old.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }

    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (rc, out) = hunter::util::run_cmd(&["git", "-C", &d, "remote", "get-url", "origin"], 30);
    assert_eq!(rc, 0);
    assert_eq!(
        out.trim(),
        "https://example.com/new.git",
        "the clone must follow the repo to its new url"
    );
}

/// A clone belonging to something else is left alone to trip the check.
#[tokio::test]
async fn changing_the_url_leaves_an_unrelated_clone_untouched() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "occupied", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/someone-else.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }

    let (status, _) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, out) = hunter::util::run_cmd(&["git", "-C", &d, "remote", "get-url", "origin"], 30);
    assert_eq!(
        out.trim(),
        "https://example.com/someone-else.git",
        "an unrelated checkout must not be rewritten -- that refusal is the point"
    );
}

/// The clone that gets repointed is the one the scheduler actually uses.
///
/// Migration 007 left any path that did not end in the repo's name
/// exactly as it was — an operator-edited one — and every scheduler
/// runner syncs `repo.path`. A repoint that derived the directory from
/// the id would rewrite nothing for such a row while `sync_repo` kept
/// finding the old origin, which is the permanent refusal this is meant
/// to avoid.
#[tokio::test]
async fn the_repoint_follows_an_operator_edited_path() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "moved", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    // A clone somewhere the id does not name, recorded on the row.
    let custom = state.config.work_root.join("elsewhere").join("checkout");
    std::fs::create_dir_all(&custom).unwrap();
    let cs = custom.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &cs, "init", "-q"],
        vec![
            "git",
            "-C",
            &cs,
            "remote",
            "add",
            "origin",
            "https://example.com/old.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }
    // Straight to the column: no production code path sets a custom
    // path, which is the point -- these rows predate the id layout.
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        state.config.db_path.to_string_lossy()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE repos SET path = ?1 WHERE id = ?2")
        .bind(&cs)
        .bind(rid)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (_, out) = hunter::util::run_cmd(&["git", "-C", &cs, "remote", "get-url", "origin"], 30);
    assert_eq!(
        out.trim(),
        "https://example.com/new.git",
        "the clone named by repo.path is the one the scheduler syncs"
    );
}

/// The scratch directory does not outlive the test that made it.
///
/// Each state carries a copy of `dev.db` plus whatever the POSTs wrote
/// under `work_root`; left behind, a full run of this suite deposits one
/// directory per test in the system temp dir, which on this machine is a
/// tmpfs shared with every build. It filled twice in one day, and a
/// truncated file mid-checkout is how that failure presents — nowhere
/// near the tests that caused it.
#[tokio::test]
async fn the_scratch_dir_is_removed_with_the_state() {
    let root = {
        let state = test_state().await;
        // Writes so the directory is not merely the db copy.
        let (status, _) = post(&state, "/api/repo/notes", json!({ "id": 1, "note": "x" })).await;
        assert_eq!(status, StatusCode::CREATED);
        state.config.root.clone()
    };
    assert!(
        !root.exists(),
        "{} outlived its test and leaks into the temp dir",
        root.display()
    );
}

/// A URL git would read as an option is refused at the door.
///
/// `git clone --upload-pack=<cmd> <dir>` runs `<cmd>`, and the clone is
/// built from whatever URL the row holds, so accepting one here is what
/// turns a repo row into an argument. The clone passes `--` as well;
/// this is the half that keeps the value out of the database.
#[tokio::test]
async fn a_url_that_looks_like_a_git_option_is_refused() {
    let state = test_state().await;

    for bad in [
        "--upload-pack=touch /tmp/pwned",
        "-u../../etc/passwd",
        "--config=core.sshCommand=id",
    ] {
        let (status, body) =
            post(&state, "/api/repos", json!({ "name": "probe", "url": bad })).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} must not be accepted; body: {body}"
        );
    }

    // A hyphen anywhere else is ordinary and must still work.
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "fine", "url": "https://github.com/acme/my-repo.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
}

// -- /api/scheduler (pause / resume) ---------------------------------------------

async fn summary_paused(state: &AppState) -> bool {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["scheduler_paused"]
        .as_bool()
        .expect("/api/summary must report scheduler_paused")
}

/// Pausing is a round trip the dashboard can see: the flag the daemon
/// loop reads, the summary field the button renders from, and the refusal
/// of a manual cycle while paused -- then all three undone by resuming.
#[tokio::test]
async fn pausing_and_resuming_the_scheduler_round_trips() {
    let state = test_state().await;
    assert!(
        !summary_paused(&state).await,
        "a fresh daemon is not paused"
    );

    let (status, body) = post(&state, "/api/scheduler", json!({ "paused": true })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, json!({ "paused": true }));
    assert!(
        state
            .scheduler
            .paused
            .load(std::sync::atomic::Ordering::SeqCst),
        "the flag the daemon loop reads must be set"
    );
    assert!(
        summary_paused(&state).await,
        "the dashboard must see the pause"
    );

    let (status, body) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body, json!({ "error": "paused" }));

    let (status, body) = post(&state, "/api/scheduler", json!({ "paused": false })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, json!({ "paused": false }));
    assert!(
        !state
            .scheduler
            .paused
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert!(!summary_paused(&state).await);
}

/// Only a JSON boolean pauses. A truthy string must not, and must not be
/// read as a resume either: the flag stays where it was.
#[tokio::test]
async fn a_non_boolean_pause_request_is_refused() {
    let state = test_state().await;
    for body in [
        json!({ "paused": "yes" }),
        json!({ "paused": 1 }),
        json!({}),
    ] {
        let (status, got) = post(&state, "/api/scheduler", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {got}");
        assert_eq!(got, json!({ "error": "paused must be a boolean" }));
    }
    assert!(
        !state
            .scheduler
            .paused
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}
