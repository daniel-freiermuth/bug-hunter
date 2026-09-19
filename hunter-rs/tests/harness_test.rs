#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Harness tests — JSONL ledger parsing, session discovery, `kill_tree`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use hunter::backends::omp_scavenge::harness;

/// Create a uniquely-named temp dir that won't collide across parallel tests.
fn tmpdir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "hunter-harness-test-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

// ============================================================
// ledger_usage — JSONL parsing
// ============================================================

#[test]
fn test_ledger_usage_sums_tokens() {
    let dir = tmpdir("lu-basic");
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
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_ledger_usage_since_filter() {
    let dir = tmpdir("lu-since");
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
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_ledger_usage_skips_non_assistant() {
    let dir = tmpdir("lu-role");
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
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_ledger_usage_handles_malformed_json() {
    let dir = tmpdir("lu-bad");
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
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_ledger_usage_missing_file() {
    let (tokens, calls) =
        harness::ledger_usage(&PathBuf::from("/nonexistent/path/does-not-exist.jsonl"), "");
    assert_eq!((tokens, calls), (0, 0));
}

#[test]
fn test_ledger_usage_empty_file() {
    let dir = tmpdir("lu-empty");
    let file = dir.join("empty.jsonl");
    fs::write(&file, "").unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    assert_eq!((tokens, calls), (0, 0));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_ledger_usage_no_usage_field() {
    let dir = tmpdir("lu-nousage");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        r#"{"timestamp":"2026-07-23T15:00:00.000Z","message":{"role":"assistant","content":"hi"}}"#,
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file, "");
    assert_eq!((tokens, calls), (0, 0));
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// snapshot
// ============================================================

#[test]
fn test_snapshot_finds_jsonl_files() {
    let dir = tmpdir("snap");
    let sub = dir.join("session-slug");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("a.jsonl"), "line1\n").unwrap();
    fs::write(sub.join("b.jsonl"), "line1\nline2\n").unwrap();
    fs::write(sub.join("c.txt"), "ignored").unwrap();

    let snap = harness::snapshot(&dir);
    assert_eq!(snap.len(), 2);
    assert!(snap.contains_key(&sub.join("a.jsonl")));
    assert!(snap.contains_key(&sub.join("b.jsonl")));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_snapshot_nonexistent_dir() {
    let snap = harness::snapshot(&PathBuf::from("/nonexistent/sessions/dir"));
    assert!(snap.is_empty());
}

// ============================================================
// discover
// ============================================================

#[test]
fn test_discover_finds_new_file() {
    let dir = tmpdir("disc-new");
    let sub = dir.join("some-slug");
    fs::create_dir_all(&sub).unwrap();

    let before: HashMap<PathBuf, u64> = HashMap::new();

    // Create a new file after snapshot
    fs::write(sub.join("session.jsonl"), r#"{"a":1}"#).unwrap();

    let result = harness::discover(&before, &dir, std::path::Path::new("/tmp/my-project"));
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().file_name().unwrap().to_str().unwrap(),
        "session.jsonl"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_discover_finds_grown_file() {
    let dir = tmpdir("disc-grow");
    let sub = dir.join("proj-slug");
    fs::create_dir_all(&sub).unwrap();

    let file_path = sub.join("session.jsonl");
    fs::write(&file_path, "short").unwrap();

    // Snapshot with original size
    let before: HashMap<PathBuf, u64> = [(file_path.clone(), 5)].into();

    // File grows
    fs::write(&file_path, "short and now longer").unwrap();

    let result = harness::discover(&before, &dir, std::path::Path::new("/tmp/work"));
    assert_eq!(result, Some(file_path));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_discover_no_changes() {
    let dir = tmpdir("disc-none");
    let sub = dir.join("slug");
    fs::create_dir_all(&sub).unwrap();
    let fp = sub.join("s.jsonl");
    fs::write(&fp, "data").unwrap();
    let before: HashMap<PathBuf, u64> = [(fp, 4)].into();

    let result = harness::discover(&before, &dir, std::path::Path::new("/work"));
    assert!(result.is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_discover_prefers_cwd_match() {
    let dir = tmpdir("disc-pref");
    let sub_a = dir.join("other-project");
    let sub_b = dir.join("my-cool-project");
    fs::create_dir_all(&sub_a).unwrap();
    fs::create_dir_all(&sub_b).unwrap();

    let before: HashMap<PathBuf, u64> = HashMap::new();

    // Both appear after snapshot
    fs::write(sub_a.join("s.jsonl"), "data").unwrap();
    // Small delay so mtime differs
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(sub_b.join("s.jsonl"), "data").unwrap();

    // cwd slug contains "my-cool-project", so sub_b should be preferred
    let result = harness::discover(
        &before,
        &dir,
        std::path::Path::new("/home/me/my-cool-project"),
    );
    assert!(result.is_some());
    let chosen = result.unwrap();
    assert!(
        chosen.to_string_lossy().contains("my-cool-project"),
        "expected cwd-matching file, got {chosen:?}"
    );
    let _ = fs::remove_dir_all(&dir);
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
