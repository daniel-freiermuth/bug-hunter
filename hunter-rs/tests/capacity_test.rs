#![allow(
    clippy::type_complexity,
    clippy::needless_pass_by_value,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
//! Capacity tests — pure ramp/retry math (§4 items 28-42) and `read_windows`
//! with agent.db fixtures (items 22-26 + edge cases).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use hunter::backends::omp_scavenge::capacity::*;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ============================================================
// ramp_7d (§4 items 28-32)
// ============================================================

#[test]
fn test_ramp_7d_none_returns_1() {
    assert_eq!(ramp_7d(None, now_ms()), 1.0);
}

#[test]
fn test_ramp_7d_expired_returns_1() {
    let now = now_ms();
    assert_eq!(ramp_7d(Some(now - 1000), now), 1.0);
}

#[test]
fn test_ramp_7d_midpoint_returns_half() {
    let now = now_ms();
    let resets = now + WEEK_MS / 2;
    let r = ramp_7d(Some(resets), now);
    assert!((r - 0.5).abs() < 1e-6, "expected ~0.5, got {r}");
}

#[test]
fn test_ramp_7d_start_returns_zero() {
    let now = now_ms();
    let resets = now + WEEK_MS;
    let r = ramp_7d(Some(resets), now);
    assert!((r - 0.0).abs() < 1e-6, "expected ~0.0, got {r}");
}

#[test]
fn test_ramp_7d_bad_data_clamped() {
    let now = now_ms();
    let resets = now + 2 * WEEK_MS;
    let r = ramp_7d(Some(resets), now);
    assert!(r <= 1.0, "expected ≤1.0, got {r}");
}

// ============================================================
// ramp_5h (§4 items 33-37)
// ============================================================

#[test]
fn test_ramp_5h_none_returns_none() {
    assert!(ramp_5h(None, now_ms()).is_none());
}

#[test]
fn test_ramp_5h_expired_returns_none() {
    let now = now_ms();
    assert!(ramp_5h(Some(now - 1000), now).is_none());
}

#[test]
fn test_ramp_5h_15min_elapsed_returns_zero() {
    let now = now_ms();
    // 15 min elapsed: resets_at = now + (5h - 15m)
    let resets = now + FIVE_H_MS - 15 * 60 * 1000;
    let r = ramp_5h(Some(resets), now).expect("should be Some");
    assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {r}");
}

#[test]
fn test_ramp_5h_halfway_returns_half() {
    let now = now_ms();
    // 2.75h elapsed: resets_at = now + (5h - 2.75h) = now + 2.25h
    let elapsed_ms = (2.75 * 3_600_000.0) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let r = ramp_5h(Some(resets), now).expect("should be Some");
    assert!((r - 0.5).abs() < 1e-6, "expected ~0.5, got {r}");
}

#[test]
fn test_ramp_5h_just_started_never_negative() {
    let now = now_ms();
    let resets = now + FIVE_H_MS; // just started, 0 elapsed
    let r = ramp_5h(Some(resets), now).expect("should be Some");
    assert!(r >= 0.0, "expected ≥0.0, got {r}");
    assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {r}");
}

// ============================================================
// retry_at_7d (§4 items 38-39)
// ============================================================

#[test]
fn test_retry_at_7d_none_returns_none() {
    assert!(retry_at_7d(None, 0.5).is_none());
}

#[test]
fn test_retry_at_7d_round_trip() {
    let now = now_ms();
    let resets = now + (0.4 * WEEK_MS as f64) as i64;
    for u in [0.0, 0.1, 0.5, 0.9] {
        let rt = retry_at_7d(Some(resets), u).expect("should be Some");
        let back = ramp_7d(Some(resets), rt as i64);
        assert!(
            (back - u).abs() < 1e-9,
            "round-trip failed for u={u}: ramp_7d(resets, retry_at_7d(resets, {u})) = {back}"
        );
    }
}

// ============================================================
// retry_at_5h (§4 items 40-42)
// ============================================================

#[test]
fn test_retry_at_5h_none_returns_none() {
    assert!(retry_at_5h(None, 0.5).is_none());
}

#[test]
fn test_retry_at_5h_round_trip() {
    let now = now_ms();
    let resets = now + (3.2 * 3_600_000.0) as i64;
    for u in [0.0, 0.25, 0.5, 0.9] {
        let rt = retry_at_5h(Some(resets), u).expect("should be Some");
        let back = ramp_5h(Some(resets), rt as i64).expect("should be Some");
        assert!(
            (back - u).abs() < 1e-9,
            "round-trip failed for u={u}: ramp_5h(resets, retry_at_5h(resets, {u})) = {back}"
        );
    }
}

#[test]
fn test_retry_at_5h_zero_equals_window_start_plus_headroom() {
    let now = now_ms();
    let resets = now + 4 * 3_600_000; // resets in 4h → elapsed 1h
    let rt = retry_at_5h(Some(resets), 0.0).expect("should be Some");
    let window_start = (resets - FIVE_H_MS) as f64;
    let expected = window_start + HEADROOM_MS as f64;
    assert!(
        (rt - expected).abs() < 1.0,
        "expected retry_at_5h(resets,0) = window_start + HEADROOM; got {rt}, expected {expected}"
    );
}

// ============================================================
// read_windows with agent.db fixture (§4 items 22-26 + edges)
// ============================================================

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_temp_path(stem: &str, ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{stem}-{}-{nanos}-{n}{ext}", std::process::id()))
}

/// Create a fixture agent.db with `usage_history` rows.
/// Each row: (`limit_id`, `used_fraction`, status, `resets_at`, `recorded_at`).
async fn make_agent_db(rows: &[(&str, Option<f64>, &str, Option<i64>, i64)]) -> PathBuf {
    use sqlx::sqlite::SqliteConnectOptions;

    let path = unique_temp_path("agent-cap-test", ".db");
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();

    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS usage_history (\
         id INTEGER PRIMARY KEY AUTOINCREMENT,\
         recorded_at INTEGER NOT NULL,\
         provider TEXT NOT NULL,\
         account_key TEXT NOT NULL,\
         limit_id TEXT NOT NULL,\
         label TEXT NOT NULL,\
         used_fraction REAL,\
         status TEXT,\
         resets_at INTEGER)",
    )
    .execute(&pool)
    .await
    .unwrap();

    for &(lid, uf, status, resets, recorded) in rows {
        sqlx::query(
            "INSERT INTO usage_history \
             (recorded_at, provider, account_key, limit_id, label, \
              used_fraction, status, resets_at) \
             VALUES (?, 'anthropic', 'acct', ?, ?, ?, ?, ?)",
        )
        .bind(recorded)
        .bind(lid)
        .bind(lid)
        .bind(uf)
        .bind(status)
        .bind(resets)
        .execute(&pool)
        .await
        .unwrap();
    }

    pool.close().await;
    path
}

#[tokio::test]
async fn test_read_windows_no_db_returns_empty() {
    let db = PathBuf::from("/nonexistent/agent.db");
    let now = now_ms();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    assert!(windows.is_empty());
}

#[tokio::test]
async fn test_read_windows_empty_table_returns_empty() {
    let db_path = make_agent_db(&[]).await;
    let now = now_ms();
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    assert!(windows.is_empty());
    let _ = std::fs::remove_file(&db_path);
}

/// §4 item 23: single live 7d row kept.
#[tokio::test]
async fn test_read_windows_keeps_active_window() {
    let now = now_ms();
    let db_path = make_agent_db(&[(
        "anthropic:7d",
        Some(0.3),
        "ok",
        Some(now + WEEK_MS / 2),
        now - 60_000,
    )])
    .await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    assert!(windows.contains_key("anthropic:7d"));
    let w = &windows["anthropic:7d"];
    assert!((w.used_fraction.unwrap() - 0.3).abs() < 1e-9);
    let _ = std::fs::remove_file(&db_path);
}

/// §4 item 22: expired per-model-class row dropped, active 7d kept.
#[tokio::test]
async fn test_read_windows_drops_expired_model_class() {
    let now = now_ms();
    let db_path = make_agent_db(&[
        (
            "anthropic:7d",
            Some(0.3),
            "ok",
            Some(now + WEEK_MS / 2),
            now - 60_000,
        ),
        (
            "anthropic:7d:fable",
            Some(0.56),
            "ok",
            Some(now - 26 * 24 * 3_600_000), // expired 26 days ago
            now - 26 * 24 * 3_600_000,
        ),
    ])
    .await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    let keys: Vec<&String> = windows.keys().collect();
    assert_eq!(keys, vec!["anthropic:7d"]);
    let _ = std::fs::remove_file(&db_path);
}

/// §4 item 24: expired 5h window rolled forward.
#[tokio::test]
async fn test_read_windows_rolls_forward_expired_5h() {
    let now = now_ms();
    // 5h window expired 47 min ago.
    let old_resets = now - 47 * 60 * 1000;
    let old_recorded = old_resets - 3_600_000; // recorded 1h before that reset
    let db_path = make_agent_db(&[(
        "anthropic:5h",
        Some(0.36),
        "ok",
        Some(old_resets),
        old_recorded,
    )])
    .await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    let w = &windows["anthropic:5h"];
    assert!((w.used_fraction.unwrap() - 0.0).abs() < 1e-9);
    assert_eq!(w.status.as_deref(), Some("ok"));
    // resets = old + _5H_MS
    assert_eq!(w.resets_at, Some(old_resets + FIVE_H_MS));
    // recorded_at = old resets (current cycle's actual start)
    assert_eq!(w.recorded_at, old_resets);
    let _ = std::fs::remove_file(&db_path);
}

/// §4 item 25: expired 7d window rolled forward.
#[tokio::test]
async fn test_read_windows_rolls_forward_expired_7d() {
    let now = now_ms();
    let old_resets = now - 2 * 3_600_000; // expired 2h ago
    let db_path = make_agent_db(&[(
        "anthropic:7d",
        Some(0.55),
        "ok",
        Some(old_resets),
        old_resets - 3_600_000,
    )])
    .await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    let w = &windows["anthropic:7d"];
    assert!((w.used_fraction.unwrap() - 0.0).abs() < 1e-9);
    assert_eq!(w.resets_at, Some(old_resets + WEEK_MS));
    assert_eq!(w.recorded_at, old_resets);
    let _ = std::fs::remove_file(&db_path);
}

/// §4 item 26: rolls forward through multiple missed cycles.
#[tokio::test]
async fn test_read_windows_rolls_forward_multiple_missed_cycles() {
    let now = now_ms();
    // 5h expired 2.3 periods ago.
    let old_resets = now - (2.3 * FIVE_H_MS as f64) as i64;
    let db_path = make_agent_db(&[(
        "anthropic:5h",
        Some(0.80),
        "ok",
        Some(old_resets),
        old_resets - 1_000_000,
    )])
    .await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    let w = &windows["anthropic:5h"];
    assert!(w.resets_at.unwrap() > now, "resets should be in the future");
    assert!(
        w.resets_at.unwrap() - now <= FIVE_H_MS,
        "resets - now should be ≤ 5h (current cycle)"
    );
    assert!((w.used_fraction.unwrap() - 0.0).abs() < 1e-9);
    let _ = std::fs::remove_file(&db_path);
}

/// NULL `resets_at` rows are kept verbatim.
#[tokio::test]
async fn test_read_windows_null_resets_kept() {
    let now = now_ms();
    let db_path = make_agent_db(&[("anthropic:5h", Some(0.10), "ok", None, now - 60_000)]).await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    let w = &windows["anthropic:5h"];
    assert_eq!(w.resets_at, None);
    assert!((w.used_fraction.unwrap() - 0.10).abs() < 1e-9);
    let _ = std::fs::remove_file(&db_path);
}

/// Some(0) for `resets_at` is treated as falsy (kept verbatim, no rollover).
#[tokio::test]
async fn test_read_windows_zero_resets_kept() {
    let now = now_ms();
    let db_path = make_agent_db(&[("anthropic:5h", Some(0.10), "ok", Some(0), now - 60_000)]).await;
    let db = db_path.clone();
    let windows = tokio::task::spawn_blocking(move || read_windows(&db, now))
        .await
        .unwrap();
    let w = &windows["anthropic:5h"];
    assert_eq!(w.resets_at, Some(0));
    assert!((w.used_fraction.unwrap() - 0.10).abs() < 1e-9);
    let _ = std::fs::remove_file(&db_path);
}

// ============================================================
// Edge cases: Some(0) behaves like None (falsy)
// ============================================================

#[test]
fn test_ramp_7d_zero_resets_returns_1() {
    assert_eq!(ramp_7d(Some(0), now_ms()), 1.0);
}

#[test]
fn test_ramp_5h_zero_resets_returns_none() {
    assert!(ramp_5h(Some(0), now_ms()).is_none());
}

#[test]
fn test_retry_at_7d_zero_returns_none() {
    assert!(retry_at_7d(Some(0), 0.5).is_none());
}

#[test]
fn test_retry_at_5h_zero_returns_none() {
    assert!(retry_at_5h(Some(0), 0.5).is_none());
}
