#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Harness tests — JSONL ledger parsing and `kill_tree`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use hunter::backends::omp_scavenge::harness;

mod support;
use support::TempDir;

// ============================================================
// ledger_usage — JSONL parsing
// ============================================================

#[test]
fn test_ledger_usage_sums_tokens() {
    let dir = TempDir::new("lu-basic");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:38:23.224Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":25}}}"#,
            "\n",
            r#"{"timestamp":"2026-07-23T15:38:24.000Z","message":{"role":"assistant","usage":{"input":200,"output":100,"cacheWrite":50}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    assert_eq!(tokens, 525); // (100+50+25) + (200+100+50)
    assert_eq!(calls, 2);
}

#[test]
fn test_ledger_usage_since_filter() {
    let dir = TempDir::new("lu-since");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:00:00.000Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":0}}}"#,
            "\n",
            r#"{"timestamp":"2026-07-23T16:00:00.000Z","message":{"role":"assistant","usage":{"input":200,"output":100,"cacheWrite":0}}}"#,
            "\n",
        ),
    )
    .unwrap();
    // Only the second record passes the filter (>= 15:30)
    let (tokens, calls) = harness::ledger_usage(&file, "2026-07-23T15:30:00.000Z");
    assert_eq!(tokens, 300);
    assert_eq!(calls, 1);
}

#[test]
fn test_ledger_usage_skips_non_assistant() {
    let dir = TempDir::new("lu-role");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:00:00.000Z","message":{"role":"user","usage":{"input":999,"output":999,"cacheWrite":999}}}"#,
            "\n",
            r#"{"timestamp":"2026-07-23T15:00:01.000Z","message":{"role":"assistant","usage":{"input":10,"output":5,"cacheWrite":0}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    // Only the assistant record counts
    assert_eq!(tokens, 15);
    assert_eq!(calls, 1);
}

#[test]
fn test_ledger_usage_handles_malformed_json() {
    let dir = TempDir::new("lu-bad");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            "not json at all\n",
            r#"{"timestamp":"2026-07-23T15:38:23.224Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":25}}}"#,
            "\n",
            "partial{{{json\n",
        ),
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    // Only the valid record counts
    assert_eq!(tokens, 175);
    assert_eq!(calls, 1);
}

#[test]
fn test_ledger_usage_missing_file() {
    let (tokens, calls) =
        harness::ledger_usage(&PathBuf::from("/nonexistent/path/does-not-exist.jsonl"), "");
    assert_eq!((tokens, calls), (0, 0));
}

#[test]
fn test_ledger_usage_empty_file() {
    let dir = TempDir::new("lu-empty");
    let file = dir.join("empty.jsonl");
    fs::write(&file, "").unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    assert_eq!((tokens, calls), (0, 0));
}

#[test]
fn test_ledger_usage_no_usage_field() {
    let dir = TempDir::new("lu-nousage");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        r#"{"timestamp":"2026-07-23T15:00:00.000Z","message":{"role":"assistant","content":"hi"}}"#,
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    assert_eq!((tokens, calls), (0, 0));
}


// ============================================================
// kill_tree — spawn a sleep, verify it dies
// ============================================================

#[test]
fn test_kill_tree_terminates_process() {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("sleep")
            .arg("999")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");

        // Still running
        assert!(child.try_wait().unwrap().is_none());

        hunter::util::kill_tree(&mut child);

        // After kill_tree, wait() should return immediately (cached status)
        let status = child.wait().expect("wait after kill_tree");
        assert!(!status.success(), "process should have been killed");
    }
}

#[test]
fn test_kill_tree_already_exited() {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("true")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");

        // Wait for it to finish naturally
        child.wait().expect("wait");

        // kill_tree on an already-exited process should not panic
        hunter::util::kill_tree(&mut child);
    }
}
