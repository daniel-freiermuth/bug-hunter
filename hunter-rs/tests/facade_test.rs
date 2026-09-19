#![allow(
    clippy::type_complexity,
    clippy::needless_pass_by_value,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
//! Facade tests — decide (§4 items 1-21, 27), unaccounted (43-48),
//! `refresh/keep_fresh` (49-57). Skip 5 server-prober tests (round 3).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use hunter::backend::{Backend, Prober, SpendLedger, Verdict};
use hunter::backends::omp_scavenge::OmpScavengeBackend;
use hunter::backends::omp_scavenge::capacity::{FIVE_H_MS, HEADROOM_MS, WEEK_MS, WindowState};
use hunter::config::Config;

// ---- constants (shared harness, test_budget.py:22-94) ---------------------

const HOUR_MS: i64 = 3_600_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---- helpers --------------------------------------------------------------

fn cfg_with(stale_after_s: f64, omp_bin: &str) -> Config {
    Config {
        root: PathBuf::from("/tmp"),
        work_root: PathBuf::from("/tmp"),
        db_path: PathBuf::from("/tmp/test.db"),
        serve_port: 8378,
        ui_dir: PathBuf::from("/tmp/ui"),
        omp_bin: omp_bin.to_owned(),
        stale_after_s,
        cache_ttl_s: 3600.0,
        poll_s: 2.0,
        session_grace_s: 120,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".to_owned(),
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

fn cfg() -> Config {
    cfg_with(1800.0, "omp")
}

/// Build a `WindowState` with sensible defaults (test_budget.py:66-78).
fn ws(lid: &str, used: f64, status: &str, resets_at: i64, age_s: f64) -> WindowState {
    let now = now_ms();
    WindowState {
        limit_id: lid.to_owned(),
        used_fraction: Some(used),
        status: Some(status.to_owned()),
        resets_at: Some(resets_at),
        recorded_at: now - (age_s * 1000.0) as i64,
        age_s,
    }
}

/// Standard healthy windows (test_budget.py:83-94).
#[allow(clippy::similar_names)]
fn healthy_windows(w5_used: f64, w5_elapsed_h: f64) -> BTreeMap<String, WindowState> {
    let now = now_ms();
    let resets_5h = now + ((5.0 - w5_elapsed_h) * HOUR_MS as f64) as i64;
    let resets_7d = now + WEEK_MS / 2;
    let mut m = BTreeMap::new();
    m.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", w5_used, "ok", resets_5h, 60.0),
    );
    m.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", resets_7d, 60.0),
    );
    m.insert(
        "anthropic:7d:model-class".to_owned(),
        ws("anthropic:7d:model-class", 0.10, "ok", resets_7d, 60.0),
    );
    m
}

// ---- FakeLedger (test_budget.py:93-100) -----------------------------------

struct FakeLedger {
    running: i64,
    finished: i64,
    /// Per-timestamp overrides for finished_since (keyed by ts_ms).
    finished_map: std::collections::HashMap<i64, i64>,
}

impl FakeLedger {
    fn new(running: i64, finished: i64) -> Self {
        Self {
            running,
            finished,
            finished_map: std::collections::HashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl SpendLedger for FakeLedger {
    async fn running_estimate(&self) -> sqlx::Result<i64> {
        Ok(self.running)
    }
    async fn finished_since(&self, ts_ms: i64) -> sqlx::Result<i64> {
        Ok(*self.finished_map.get(&ts_ms).unwrap_or(&self.finished))
    }
    async fn finished_between(&self, _start_ms: i64, _end_ms: i64) -> sqlx::Result<i64> {
        Ok(0)
    }
    async fn log_window_observation(
        &self,
        _lid: &str,
        _uf: Option<f64>,
        _s: Option<&str>,
        _r: Option<i64>,
        _a: f64,
    ) -> sqlx::Result<()> {
        Ok(())
    }
    async fn last_window_observation(
        &self,
        _lid: &str,
        _r: i64,
    ) -> sqlx::Result<Option<(i64, f64)>> {
        Ok(None)
    }
    async fn record_calibration_sample(
        &self,
        _lid: &str,
        _wr: Option<i64>,
        _d: f64,
        _t: i64,
    ) -> sqlx::Result<()> {
        Ok(())
    }
    async fn estimate_capacity(&self, _lid: &str) -> sqlx::Result<Option<f64>> {
        Ok(None)
    }
}

// ---- FakeProber -----------------------------------------------------------

struct FakeProber {
    calls: Mutex<Vec<(Vec<String>, u64)>>,
    rcs: Vec<i32>,
}

impl FakeProber {
    fn new(rc: i32) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            rcs: vec![rc],
        }
    }
}

impl Prober for FakeProber {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String) {
        let mut calls = self.calls.lock().unwrap();
        let idx = calls.len();
        calls.push((
            argv.iter().map(std::string::ToString::to_string).collect(),
            timeout_s,
        ));
        let rc = self
            .rcs
            .get(idx)
            .copied()
            .unwrap_or(*self.rcs.last().unwrap_or(&0));
        (rc, String::new())
    }
}

// ---- backend builder ------------------------------------------------------

fn make_backend(ledger: FakeLedger) -> OmpScavengeBackend {
    make_backend_with(ledger, cfg(), FakeProber::new(0))
}

fn make_backend_with(ledger: FakeLedger, cfg: Config, prober: FakeProber) -> OmpScavengeBackend {
    OmpScavengeBackend {
        cfg,
        ledger: Arc::new(ledger),
        agent_db: PathBuf::from("/nonexistent/agent.db"),
        prober: Arc::new(prober),
    }
}

// ---- verdict helpers ------------------------------------------------------

fn is_granted(v: &Verdict) -> bool {
    matches!(v, Verdict::Granted { .. })
}

fn is_denied(v: &Verdict) -> bool {
    matches!(v, Verdict::Denied { .. })
}

fn reason(v: &Verdict) -> &str {
    match v {
        Verdict::Granted { reason, .. } | Verdict::Denied { reason, .. } => reason,
    }
}

fn cap_tokens(v: &Verdict) -> Option<i64> {
    match v {
        Verdict::Granted { cap_tokens, .. } => *cap_tokens,
        Verdict::Denied { .. } => None,
    }
}

fn retry_at(v: &Verdict) -> Option<f64> {
    match v {
        Verdict::Denied { retry_at, .. } => *retry_at,
        Verdict::Granted { .. } => None,
    }
}

// ============================================================
// Decide tests (§4 items 1-21, 27)
// ============================================================

/// Item 1: empty windows deny.
#[tokio::test]
async fn test_empty_windows_deny() {
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&BTreeMap::new(), 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("no window data"));
}

/// Item 2: stale 5h low usage allows via ramp, not bypass.
#[tokio::test]
async fn test_stale_5h_low_usage_allows_via_ramp() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.10, "ok", now + HOUR_MS, 3600.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(
        is_granted(&o.normal),
        "expected Granted, got {:?}",
        o.normal
    );
}

/// Item 3: stale 5h high usage denies.
#[tokio::test]
async fn test_stale_5h_high_usage_denies() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.90, "ok", now + HOUR_MS, 3600.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

/// Item 4: own finished jobs count toward `effective_used`.
#[tokio::test]
async fn test_own_finished_jobs_count() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.10, "ok", now + HOUR_MS, 30.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 1_600_000));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

/// Item 5: denied by 7d ramp when 7d has high usage.
#[tokio::test]
async fn test_denied_by_7d_ramp() {
    let now = now_ms();
    let mut w = healthy_windows(0.05, 4.5);
    // Override 7d: high usage, tight ramp.
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.30,
            "ok",
            now + (0.95 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 6: 5h and 7d unaccounted reservations are independent.
#[tokio::test]
async fn test_unaccounted_reservations_independent_7d() {
    let now = now_ms();
    // (a) 7d .02 resets now+.9w → ramp .10; finished=20M → res_7d≈.298 → Denied 7d.
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.0,
            "ok",
            now + (0.5 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.02,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 20_000_000));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 6b: independent reservations — 5h denied when 5h ramp is the gate.
#[tokio::test]
async fn test_unaccounted_reservations_independent_5h() {
    let now = now_ms();
    // (b) 5h used 0, elapsed 3h (ramp .556); finished=5M → res_5h=2.5 → Denied 5h.
    let elapsed_3h = now + (2.0 * HOUR_MS as f64) as i64; // resets in 2h → 3h elapsed
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.0, "ok", elapsed_3h, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.02, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 5_000_000));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

/// Item 7: 7d used above ramp → Denied; `retry_at` round-trip.
#[tokio::test]
async fn test_7d_used_above_ramp_deny() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.30,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("ramp"));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    let expected = now as f64 + 0.20 * WEEK_MS as f64;
    assert!(
        (rt - expected).abs() < 2000.0,
        "retry_at off: got {rt}, expected ≈{expected}"
    );
}

/// Item 8: 7d used below ramp → Granted.
#[tokio::test]
async fn test_7d_used_below_ramp_allow() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.30, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 9: 5h first 30min deny (headroom).
#[tokio::test]
async fn test_5h_first_30min_deny() {
    let now = now_ms();
    // elapsed 0.25h → ramp 0.
    let elapsed_ms = (0.25 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.05, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
    assert!(reason(&o.normal).contains("ramp"));
}

/// Item 10: 5h at exactly 30min → deny (ramp=0, used .01 ≥ 0).
#[tokio::test]
async fn test_5h_at_exactly_30min_deny() {
    let now = now_ms();
    let elapsed_ms = HEADROOM_MS; // exactly 30 min
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.01, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
}

/// Item 11: 5h halfway low usage → Granted.
#[tokio::test]
async fn test_5h_halfway_low_usage_allow() {
    let now = now_ms();
    // 2.75h elapsed → ramp 0.5.
    let elapsed_ms = (2.75 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.05, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 12: 5h halfway high usage → Denied; `retry_at`.
#[tokio::test]
async fn test_5h_halfway_high_usage_deny() {
    let now = now_ms();
    let elapsed_ms = (2.75 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.60, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
    assert!(reason(&o.normal).contains("ramp"));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    // retry_at ≈ NOW + 0.45 * 3600 * 1000 (ramp inverse of 0.60)
    let expected = now as f64 + 0.45 * HOUR_MS as f64;
    assert!(
        (rt - expected).abs() < 5000.0,
        "retry_at off: got {rt}, expected ≈{expected}"
    );
}

/// Item 13: 5h nearly done, high usage → Granted.
#[tokio::test]
async fn test_5h_end_high_usage_allow() {
    let now = now_ms();
    // 4.95h elapsed → ramp ≈ 0.989.
    let elapsed_ms = (4.95 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.90, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 14: 5h exhausted → Denied; `retry_at` == `resets_at` exactly.
#[tokio::test]
async fn test_5h_exhausted_deny() {
    let now = now_ms();
    let resets = now + WEEK_MS / 2;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 1.0, "exhausted", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    assert!(
        (rt - resets as f64).abs() < 1.0,
        "retry_at should equal resets_at"
    );
}

/// Item 15: 5h exhausted but stale still denies.
#[tokio::test]
async fn test_5h_exhausted_stale_still_denies() {
    let now = now_ms();
    let resets = now + 4 * 60 * 1000; // resets in 4 min
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 1.0, "exhausted", resets, 3600.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    assert!(
        (rt - resets as f64).abs() < 1.0,
        "retry_at should equal resets_at"
    );
}

/// Item 16: 7d denial during 5h headroom uses 7d retry.
#[allow(clippy::similar_names)]
#[tokio::test]
async fn test_7d_denial_during_5h_headroom() {
    let now = now_ms();
    // 5h in headroom (elapsed 0.25h), 7d over ramp.
    let resets_5h = now + (4.75 * HOUR_MS as f64) as i64;
    let resets_7d = now + (0.9 * WEEK_MS as f64) as i64;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.0, "ok", resets_5h, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.30, "ok", resets_7d, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(
        reason(&o.normal).starts_with("anthropic:7d"),
        "reason should start with 7d lid: {}",
        reason(&o.normal)
    );
    let rt = retry_at(&o.normal).expect("retry_at");
    let expected = now as f64 + 0.20 * WEEK_MS as f64;
    assert!((rt - expected).abs() < 2000.0);
}

/// Item 17: no 5h window, only 7d(.10) → Granted.
#[tokio::test]
async fn test_no_5h_window_allow() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
    assert!(cap_tokens(&o.normal).is_some_and(|c| c > 0));
}

/// Item 18: no 5h, 7d over ramp → Denied.
#[tokio::test]
async fn test_no_5h_window_7d_over_deny() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.30,
            "ok",
            now + (0.95 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 19: expired model-class window ignored.
#[tokio::test]
async fn test_expired_model_class_window_ignored() {
    let now = now_ms();
    let mut w = healthy_windows(0.05, 4.5);
    // Add expired model-class window.
    let expired_resets = now - 3 * WEEK_MS;
    w.insert(
        "anthropic:7d:abandoned-model".to_owned(),
        WindowState {
            limit_id: "anthropic:7d:abandoned-model".to_owned(),
            used_fraction: Some(0.99),
            status: Some("ok".to_owned()),
            resets_at: Some(expired_resets),
            recorded_at: now - 26 * 24 * HOUR_MS,
            age_s: 26.0 * 24.0 * 3600.0,
        },
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 20: active model-class window still gates.
#[tokio::test]
async fn test_active_model_class_window_gates() {
    let now = now_ms();
    let mut w = healthy_windows(0.05, 4.5);
    w.insert(
        "anthropic:7d:active-model".to_owned(),
        ws(
            "anthropic:7d:active-model",
            0.30,
            "ok",
            now + (0.95 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 21: healthy windows → Granted with cap > 0.
#[tokio::test]
async fn test_healthy_allow() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
    assert!(cap_tokens(&o.normal).is_some_and(|c| c > 0));
}

/// Item 27: fresh rollover + unaccounted alone → Denied.
#[tokio::test]
async fn test_decide_denies_on_unaccounted_alone_through_rollover() {
    let now = now_ms();
    // 5h: used 0, rolled over, elapsed 3h (ramp .556).
    let resets = now + 2 * HOUR_MS; // resets in 2h → elapsed 3h
    let window_start = resets - FIVE_H_MS;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        WindowState {
            limit_id: "anthropic:5h".to_owned(),
            used_fraction: Some(0.0),
            status: Some("ok".to_owned()),
            resets_at: Some(resets),
            recorded_at: window_start, // probe at window start
            age_s: (now - window_start) as f64 / 1000.0,
        },
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    // ledger finished=2_900_000 → res_5h ≈ 1.45 → eff 1.45 ≥ .556 → Denied.
    let b = make_backend(FakeLedger::new(0, 2_900_000));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

// ============================================================
// Unaccounted tests (§4 items 43-48)
//
// These test _unaccounted_fraction indirectly through decide_with_windows.
// FakeLedger ignores ts in finished_since, so the per-window probe_at
// distinction doesn't affect the RETURNED fraction — we verify by checking
// whether the resulting verdict grants or denies.
// ============================================================

/// Item 43: no jobs → unaccounted (0, 0) → Granted.
#[tokio::test]
async fn test_unaccounted_no_jobs() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 44: running job counted via `cap_tokens`.
#[tokio::test]
async fn test_unaccounted_running_job() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(150_000, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    // With 150k running and 2M cap, res ≈ 0.075 — still granted.
    assert!(is_granted(&o.normal));
}

/// Item 45: anticipated added to both fields.
#[tokio::test]
async fn test_unaccounted_anticipated() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 80_000).await.unwrap();
    // With anticipated 80k and 2M cap, res ≈ 0.04 — still granted.
    assert!(is_granted(&o.normal));
}

/// Item 46: finished job scoped to each window's own probe.
#[tokio::test]
async fn test_unaccounted_finished_job_probe_scope() {
    let w = healthy_windows(0.05, 4.5);
    // FakeLedger always returns finished=50_000 regardless of ts.
    let b = make_backend(FakeLedger::new(0, 50_000));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    // res ≈ 50k/2M = 0.025 — small, still granted.
    assert!(is_granted(&o.normal));
}

/// Item 47: stale 7d probe doesn't drag 5h baseline back.
/// (Verifies per-window `probe_at`, not shared min.)
#[tokio::test]
async fn test_unaccounted_stale_7d_probe() {
    // With FakeLedger (finished_since ignores ts), we can't
    // differentiate per-window probe_at. This test verifies the
    // structure doesn't break; real Store tests (58-66) cover the delta.
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 48: fallback to min when window missing.
#[tokio::test]
async fn test_unaccounted_fallback_min_probe() {
    let now = now_ms();
    // Only 7d present (probe 1h ago); 10k tok after → (10k, 10k).
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 3600.0),
    );
    let b = make_backend(FakeLedger::new(0, 10_000));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    // 10k on 67.2M cap is tiny → Granted.
    assert!(is_granted(&o.normal));
}

// ============================================================
// Refresh / keep_fresh tests (§4 items 49-57)
// ============================================================

static DB_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_path(stem: &str, ext: &str) -> PathBuf {
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{stem}-{}-{nanos}-{n}{ext}", std::process::id()))
}

/// Create a fixture agent.db for `keep_fresh` tests. Windows are
/// controlled by inserting `usage_history` rows.
async fn make_agent_db(rows: &[(&str, Option<f64>, &str, Option<i64>, i64)]) -> PathBuf {
    use sqlx::sqlite::SqliteConnectOptions;

    let path = temp_path("agent-facade-test", ".db");
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

// The prober is behind Arc<dyn Prober>, so we can't downcast to check
// calls. Use a shared-state approach:

struct SharedProber {
    calls: Arc<Mutex<Vec<(Vec<String>, u64)>>>,
    rcs: Vec<i32>,
}

impl SharedProber {
    fn new(rc: i32) -> (Self, Arc<Mutex<Vec<(Vec<String>, u64)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                rcs: vec![rc],
            },
            calls,
        )
    }
    fn with_rcs(rcs: Vec<i32>) -> (Self, Arc<Mutex<Vec<(Vec<String>, u64)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                rcs,
            },
            calls,
        )
    }
}

impl Prober for SharedProber {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String) {
        let mut calls = self.calls.lock().unwrap();
        let idx = calls.len();
        calls.push((
            argv.iter().map(std::string::ToString::to_string).collect(),
            timeout_s,
        ));
        let rc = self
            .rcs
            .get(idx)
            .copied()
            .unwrap_or(*self.rcs.last().unwrap_or(&0));
        (rc, String::new())
    }
}

/// Item 49 (re-done with shared prober): no windows → True; calls == [INVALIDATE, READ].
#[tokio::test]
async fn test_no_windows_forces_probe_shared() {
    let db = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(result);
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 2);
    assert!(c[0].0.contains(&"invalidate".to_string()));
    assert!(!c[1].0.contains(&"invalidate".to_string()));
    let _ = std::fs::remove_file(&db);
}

/// Item 50: fresh window → no probe.
#[tokio::test]
async fn test_fresh_window_no_probe() {
    let now = now_ms();
    let db = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 60_000, // age 60s, fresh
    )])
    .await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(), // stale_after_s = 1800
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    assert_eq!(calls.lock().unwrap().len(), 0);
    let _ = std::fs::remove_file(&db);
}

/// Item 51: stale window → probe.
#[tokio::test]
async fn test_stale_window_forces_probe() {
    let now = now_ms();
    let db = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 2_000_000, // age 2000s > 1800
    )])
    .await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(result);
    assert_eq!(calls.lock().unwrap().len(), 2);
    let _ = std::fs::remove_file(&db);
}

/// Item 52: age just inside `stale_after_s` → fresh, no probe. The exact
/// `<=` boundary is pinned by `test_age_exactly_at_threshold_is_fresh`.
#[tokio::test]
async fn test_age_just_inside_threshold_no_probe() {
    let now = now_ms();
    let db = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 1_800_000 + 5_000, // comfortably inside threshold (5s margin for CI)
    )])
    .await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    assert_eq!(calls.lock().unwrap().len(), 0);
    let _ = std::fs::remove_file(&db);
}

/// Item 52 (boundary): an age landing EXACTLY on `stale_after_s` is fresh,
/// one millisecond past it is not. Goes through `is_fresh` — the predicate
/// `keep_fresh` gates the probe on — with `age_s` injected directly, because
/// the agent.db path derives `age_s` from the clock read inside `keep_fresh`
/// and so can never land on the threshold deterministically.
#[test]
fn test_age_exactly_at_threshold_is_fresh() {
    let now = now_ms();
    let (prober, _calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(), // stale_after_s = 1800
        ledger: Arc::new(FakeLedger::new(0, 0)),
        // Never read: is_fresh inspects the passed-in windows only.
        agent_db: PathBuf::from("/nonexistent/agent.db"),
        prober: Arc::new(prober),
    };

    let at = |age_s| {
        BTreeMap::from([(
            "anthropic:5h".to_owned(),
            ws("anthropic:5h", 0.10, "ok", now + HOUR_MS, age_s),
        )])
    };

    assert!(
        b.is_fresh(&at(1800.0)),
        "age == stale_after_s is fresh (inclusive ≤), so keep_fresh skips the probe"
    );
    assert!(
        !b.is_fresh(&at(1800.001)),
        "one millisecond past the threshold is stale"
    );
}

/// Item 53: respects configured `stale_after_s`.
#[tokio::test]
async fn test_respects_configured_stale_after_s() {
    let now = now_ms();
    let db = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 500_000, // age 500s
    )])
    .await;

    // threshold 1800 → 500s is fresh
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg_with(1800.0, "omp"),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    assert_eq!(calls.lock().unwrap().len(), 0);

    // threshold 300 → 500s is stale
    let (prober2, calls2) = SharedProber::new(0);
    let b2 = OmpScavengeBackend {
        cfg: cfg_with(300.0, "omp"),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober2),
    };
    let result2 = b2.keep_fresh().await.unwrap();
    assert!(result2);
    assert_eq!(calls2.lock().unwrap().len(), 2);
    let _ = std::fs::remove_file(&db);
}

/// Item 54: invalidates before reading.
#[tokio::test]
async fn test_invalidates_before_reading() {
    let db = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    b.keep_fresh().await.unwrap();
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 2);
    assert!(
        c[0].0.contains(&"invalidate".to_string()),
        "first call should be invalidate"
    );
    assert!(
        !c[1].0.contains(&"invalidate".to_string()),
        "second call should be read"
    );
    let _ = std::fs::remove_file(&db);
}

/// Item 55: uses configured `omp_bin`.
#[tokio::test]
async fn test_uses_configured_omp_bin() {
    let db = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg_with(1800.0, "/custom/path/omp"),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    b.keep_fresh().await.unwrap();
    let c = calls.lock().unwrap();
    assert_eq!(c[0].0[0], "/custom/path/omp");
    assert_eq!(c[1].0[0], "/custom/path/omp");
    let _ = std::fs::remove_file(&db);
}

/// Item 56: failed probe → false.
#[tokio::test]
async fn test_failed_probe_returns_false() {
    let db = make_agent_db(&[]).await;
    let (prober, _calls) = SharedProber::new(1); // rc=1 for both
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    let _ = std::fs::remove_file(&db);
}

/// Item 57: failed invalidate does not block the read attempt.
#[tokio::test]
async fn test_failed_invalidate_does_not_block_read() {
    let db = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::with_rcs(vec![1, 0]); // invalidate rc=1, read rc=0
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(result);
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 2, "both calls should be attempted");
    let _ = std::fs::remove_file(&db);
}

// ============================================================
// Prioritized verdict tests (monotonicity / prio override)
// ============================================================

/// Both normal and prioritized denied when empty windows.
#[tokio::test]
async fn test_prio_both_denied_empty_windows() {
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&BTreeMap::new(), 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(is_denied(&o.prioritized));
}

/// Normal granted → prioritized == normal (or upgraded cap).
#[tokio::test]
async fn test_prio_granted_when_normal_granted() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_granted(&o.normal));
    assert!(is_granted(&o.prioritized));
    // Prio headroom should be ≥ normal headroom.
    let nc = cap_tokens(&o.normal);
    let pc = cap_tokens(&o.prioritized);
    assert!(pc >= nc, "prio cap {pc:?} should be >= normal cap {nc:?}");
}

/// Normal denied by 7d pacing → prioritized gets prio override.
#[tokio::test]
async fn test_prio_override_7d_pacing() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    // 7d slightly over ramp: used .11, ramp .10 (resets near start).
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.11,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal), "normal should be denied");
    assert!(
        is_granted(&o.prioritized),
        "prioritized should be granted (prio override)"
    );
    assert!(
        reason(&o.prioritized).contains("prio override"),
        "reason should contain 'prio override': {}",
        reason(&o.prioritized)
    );
}

/// Normal denied by 5h pacing → prioritized gets prio override.
#[tokio::test]
async fn test_prio_override_5h_pacing() {
    let now = now_ms();
    // 5h halfway, usage just over ramp.
    let elapsed_ms = (2.75 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.55, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(
        is_granted(&o.prioritized),
        "prioritized should be granted: {:?}",
        o.prioritized
    );
}

/// 7d exhausted → prio NOT waived (exhaustion beats prio).
#[tokio::test]
async fn test_prio_exhausted_not_waived() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 1.0, "exhausted", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(
        is_denied(&o.prioritized),
        "exhaustion should not be waived by prio"
    );
}

// ============================================================
// effective_used tests
// ============================================================

#[test]
fn test_effective_used_exhausted() {
    use hunter::backends::omp_scavenge::capacity::effective_used;
    let w = WindowState {
        limit_id: "anthropic:5h".to_owned(),
        used_fraction: Some(0.5),
        status: Some("exhausted".to_owned()),
        resets_at: Some(99999),
        recorded_at: 1000,
        age_s: 10.0,
    };
    // Exhausted: used clamped to 1.0, plus reservation.
    assert!((effective_used(&w, 0.1) - 1.1).abs() < 1e-9);
}

#[test]
fn test_effective_used_normal() {
    use hunter::backends::omp_scavenge::capacity::effective_used;
    let w = WindowState {
        limit_id: "anthropic:5h".to_owned(),
        used_fraction: Some(0.3),
        status: Some("ok".to_owned()),
        resets_at: Some(99999),
        recorded_at: 1000,
        age_s: 10.0,
    };
    assert!((effective_used(&w, 0.05) - 0.35).abs() < 1e-9);
}
