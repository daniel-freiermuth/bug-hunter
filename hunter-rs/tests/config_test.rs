#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
//! Config loading — the `load()` validation gate. Every rejection here
//! exists because the bad value is accepted silently by serde and only
//! surfaces as a worker that dies on its first meter tick, or a budget
//! probe that never stops firing.

use std::path::Path;

use hunter::config::Config;

mod support;
use support::TempDir;

/// Write `config.json` under a scratch root and load it.
fn load(dir: &TempDir, json: &str) -> anyhow::Result<Config> {
    std::fs::write(dir.join("config.json"), json).expect("write config.json");
    Config::load(dir.path())
}

fn err(dir: &TempDir, json: &str) -> String {
    match load(dir, json) {
        Ok(_) => panic!("expected {json} to be rejected"),
        Err(e) => format!("{e:#}"),
    }
}

// ---------------------------------------------------------------------------
// Worker limits: zero is the tightest cap, not "disabled"
// ---------------------------------------------------------------------------

#[test]
fn test_hunt_cap_new_tokens_zero_rejected() {
    let dir = TempDir::new("cfg-hunt-cap-zero");
    let msg = err(&dir, r#"{"hunt": {"capNewTokens": 0}}"#);
    assert!(
        msg.contains("hunt.capNewTokens") && msg.contains("got 0"),
        "message must name field and value: {msg}"
    );
}

#[test]
fn test_hunt_cap_new_tokens_negative_rejected() {
    let dir = TempDir::new("cfg-hunt-cap-neg");
    let msg = err(&dir, r#"{"hunt": {"capNewTokens": -1}}"#);
    assert!(
        msg.contains("hunt.capNewTokens") && msg.contains("got -1"),
        "message must name field and value: {msg}"
    );
}

#[test]
fn test_hunt_max_wall_s_zero_rejected() {
    let dir = TempDir::new("cfg-hunt-wall-zero");
    let msg = err(&dir, r#"{"hunt": {"maxWallS": 0}}"#);
    assert!(msg.contains("hunt.maxWallS"), "{msg}");
}

#[test]
fn test_fix_cap_new_tokens_negative_rejected() {
    let dir = TempDir::new("cfg-fix-cap-neg");
    let msg = err(&dir, r#"{"fix": {"capNewTokens": -5000}}"#);
    assert!(
        msg.contains("fix.capNewTokens") && msg.contains("got -5000"),
        "{msg}"
    );
}

#[test]
fn test_fix_max_wall_s_zero_rejected() {
    let dir = TempDir::new("cfg-fix-wall-zero");
    let msg = err(&dir, r#"{"fix": {"maxWallS": 0}}"#);
    assert!(msg.contains("fix.maxWallS"), "{msg}");
}

// ---------------------------------------------------------------------------
// budget.staleAfterS: negatives make every window permanently stale, but
// zero is a defined threshold (`age_s <= stale_after_s`).
// ---------------------------------------------------------------------------

#[test]
fn test_stale_after_s_negative_rejected() {
    let dir = TempDir::new("cfg-stale-neg");
    let msg = err(&dir, r#"{"budget": {"staleAfterS": -1}}"#);
    assert!(
        msg.contains("budget.staleAfterS") && msg.contains("-1"),
        "{msg}"
    );
}

#[test]
fn test_stale_after_s_zero_accepted() {
    let dir = TempDir::new("cfg-stale-zero");
    let cfg = load(
        &dir,
        r#"{"budget": {"staleAfterS": 0},
             "hunt": {"capNewTokens": 1, "maxWallS": 1},
             "fix": {"capNewTokens": 1, "maxWallS": 1}}"#,
    )
    .expect("staleAfterS 0 is a valid threshold");
    assert_eq!(cfg.stale_after_s, 0.0);
    assert_eq!(cfg.hunt_cap_tokens, 1);
    assert_eq!(cfg.fix_max_wall_s, 1);
}

#[test]
fn test_defaults_load_without_config_file() {
    let dir = TempDir::new("cfg-defaults");
    let cfg = Config::load(dir.path()).expect("missing config.json = all defaults");
    assert_eq!(cfg.hunt_cap_tokens, 200_000);
    assert_eq!(cfg.hunt_max_wall_s, 1800);
    assert_eq!(cfg.fix_cap_tokens, 150_000);
    assert_eq!(cfg.fix_max_wall_s, 2700);
    assert_eq!(cfg.stale_after_s, 300.0);
    assert_eq!(cfg.root, Path::new(dir.path()));
}

/// Cadences are validated at the precision the scheduler uses.
///
/// `scan.intervalDays` is multiplied by `86_400_000` and cast to `i64`;
/// the modernization and standards cadences are cast to `i64` days
/// first. So a value that rounds to a non-positive interval does not
/// mean "never" or "rarely" — it makes that job eligible on every cycle,
/// the opposite of what a small number is meant to express.
#[test]
fn cadences_that_round_to_no_interval_are_rejected() {
    for (field, value) in [
        // Truncates to 0 days -> eligible every cycle.
        ("modernization", "0.5"),
        ("standards", "0.5"),
        ("modernization", "-0.5"),
        // Zero and negative outright.
        ("modernization", "0"),
        ("standards", "-1"),
        ("scan", "0"),
        ("scan", "-1"),
        // Sub-millisecond: * 86_400_000 still floors to 0 ms.
        ("scan", "0.00000000001"),
        // Would overflow the millisecond conversion.
        ("modernization", "1e300"),
        ("scan", "1e300"),
    ] {
        let dir = TempDir::new("cfg-cadence");
        std::fs::write(
            dir.join("config.json"),
            format!(r#"{{"{field}": {{"intervalDays": {value}}}}}"#),
        )
        .unwrap();
        let err = Config::load(dir.path())
            .expect_err(&format!("{field}.intervalDays = {value} was accepted"));
        assert!(
            format!("{err}").contains(&format!("{field}.intervalDays")),
            "error should name the field, got: {err}"
        );
    }
}

/// The values an operator actually writes still load.
#[test]
fn ordinary_cadences_load() {
    let dir = TempDir::new("cfg-cadence-ok");
    std::fs::write(
        dir.join("config.json"),
        r#"{"scan": {"intervalDays": 0.5},
            "modernization": {"intervalDays": 30},
            "standards": {"intervalDays": 14}}"#,
    )
    .unwrap();
    let cfg = Config::load(dir.path()).expect("valid cadences");
    // Half a day is meaningful for the scan scheduler: it keeps a
    // sub-day interval because that one is not truncated to whole days.
    assert!((cfg.scan_interval_days - 0.5).abs() < f64::EPSILON);
    assert_eq!(cfg.modernization_interval_days, 30);
    assert_eq!(cfg.standards_interval_days, 14);
}
