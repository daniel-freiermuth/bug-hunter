//! Worker runner — spawns headless omp, meters its ledger, kills at cap.
//!
//! Port of `hunter/backends/omp_scavenge/harness.py`. Each worker receives
//! a private OMP session directory so its meter can never observe another
//! session's ledger.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::types::RunResult;
use crate::util::kill_tree;


// ---------------------------------------------------------------------------
// Ledger metering (harness.py:27-55)
// ---------------------------------------------------------------------------

/// Sum "new" tokens (input + output + cacheWrite) and call count from a
/// session JSONL file, optionally filtering to records at or after
/// `since_iso` (lexicographic compare on the `timestamp` field).
///
/// `OSError` / IO errors → (0, 0).
pub fn ledger_usage(session_file: &Path, since_iso: &str) -> (i64, i64) {
    let Ok(file) = fs::File::open(session_file) else {
        return (0, 0);
    };
    let reader = BufReader::new(file);
    let mut tokens: i64 = 0;
    let mut calls: i64 = 0;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let rec: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // partial trailing line mid-write
        };
        if !since_iso.is_empty() {
            let ts = rec.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
            if ts < since_iso {
                continue;
            }
        }
        let Some(msg) = rec.get("message").and_then(|v| v.as_object()) else {
            continue;
        };
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(u) = msg.get("usage").and_then(|v| v.as_object()) {
            calls += 1;
            let inp = u
                .get("input")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let out = u
                .get("output")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let cw = u
                .get("cacheWrite")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            tokens += inp + out + cw;
        }
    }
    (tokens, calls)
}


/// A worker gets an exclusive OMP session directory.  Unlike the shared
/// default session tree, every JSONL below this directory belongs to this
/// worker, so no fuzzy cwd matching or growth heuristic is needed.
fn worker_session_dir(cfg: &Config) -> std::io::Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = cfg
        .work_root
        .join("sessions")
        .join(format!("worker-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Find the worker's own session ledger in its exclusive directory.
fn private_session_file(dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            candidates.push(path);
        } else if entry.file_type().ok().is_some_and(|kind| kind.is_dir()) {
            for child in fs::read_dir(path).ok()?.flatten() {
                let path = child.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                    candidates.push(path);
                }
            }
        }
    }
    candidates
        .into_iter()
        .max_by_key(|path| path.metadata().ok().and_then(|metadata| metadata.modified().ok()))
}


// ---------------------------------------------------------------------------
// ISO timestamp helper (replaces Python time.strftime + gmtime)
// ---------------------------------------------------------------------------

/// Convert epoch seconds to "YYYY-MM-DDTHH:MM:SS.000Z" (UTC).
/// Uses the Howard Hinnant civil-from-days algorithm.
#[allow(clippy::many_single_char_names)]
fn epoch_to_iso(secs: u64) -> String {
    let s = secs % 60;
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = (secs / 86400) as i64;

    // Civil date from days since 1970-01-01
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

// ---------------------------------------------------------------------------
// Worker output capture
// ---------------------------------------------------------------------------

/// Bytes of worker output kept for `RunResult::stdout_tail`.
const TAIL_BYTES: usize = 2000;

/// Drain a worker pipe on its own thread, keeping only the last
/// `TAIL_BYTES`. The pipe has to be read for as long as the worker runs: a
/// full pipe buffer blocks the worker, and a blocked worker stops writing
/// the ledger the cap watchdog reads. Everything before the tail is
/// dropped as it arrives — a chatty worker emits megabytes.
fn drain_tail<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        if let Some(mut p) = pipe {
            let mut chunk = [0u8; 8192];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > TAIL_BYTES {
                            tail.drain(..tail.len() - TAIL_BYTES);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        tail
    })
}

/// Last `TAIL_BYTES` of `s`, snapped forward to a `char` boundary.
fn tail_str(s: &str) -> &str {
    crate::util::tail(s, TAIL_BYTES)
}

// ---------------------------------------------------------------------------
// Worker execution (harness.py:103-163)
// ---------------------------------------------------------------------------

/// Spawn `omp -p`, meter its JSONL session ledger, kill at cap.
///
/// Blocking — call via `spawn_blocking` from async contexts.
pub fn run_worker(
    cfg: &Config,
    cwd: &Path,
    prompt: &str,
    cap_tokens: i64,
    max_wall_s: i64,
    model: Option<&str>,
) -> RunResult {
    let t0 = Instant::now();
    let session_dir = match worker_session_dir(cfg) {
        Ok(dir) => dir,
        Err(err) => {
            return RunResult {
                exit_code: Some(127),
                killed_reason: Some("unmetered".to_owned()),
                tokens_new: 0,
                calls: 0,
                session_file: None,
                duration_s: 0.0,
                stdout_tail: format!("creating worker session directory failed: {err}"),
                usage_delta: None,
            };
        }
    };

    let spawn_iso = {
        let epoch_s = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        epoch_to_iso(epoch_s)
    };

    // Build command: [omp_bin, "-p", prompt] + optional flags
    let mut cmd = Command::new(&cfg.omp_bin);
    cmd.arg("-p").arg(prompt);
    cmd.arg(format!("--session-dir={}", session_dir.display()));
    if let Some(m) = model {
        cmd.arg(format!("--model={m}"));
    }
    if let Some(ref smol) = cfg.model_smol {
        cmd.arg(format!("--smol={smol}"));
    }

    // Spawn in its own process group (for group-kill).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Build a minimal env so the worker doesn't inherit the full daemon env
    // (especially ANTHROPIC_ API keys/tokens).
    let allowlist: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SSH_AUTH_SOCK",
    ];
    let allowed_prefixes: &[&str] = &["OMP_", "XDG_"];
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if allowlist.contains(&k.as_str()) || allowed_prefixes.iter().any(|p| k.starts_with(p)) {
            cmd.env(&k, &v);
        }
    }
    cmd.current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        Err(e) => {
            return RunResult {
                exit_code: Some(127),
                killed_reason: None,
                tokens_new: 0,
                calls: 0,
                session_file: None,
                duration_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                stdout_tail: format!("spawn error: {e}"),
                usage_delta: None,
            };
        }
    };

    // Readers start here and run until the pipes close: see `drain_tail`.
    let so = drain_tail(proc.stdout.take());
    let se = drain_tail(proc.stderr.take());

    let mut session: Option<PathBuf> = None;
    let mut tokens: i64 = 0;
    let mut calls: i64 = 0;
    let mut killed: Option<String> = None;

    loop {
        let exited = matches!(proc.try_wait(), Ok(Some(_)));
        if session.is_none() {
            session = private_session_file(&session_dir);
        }
        if let Some(ref sess) = session {
            let (t, c) = ledger_usage(sess, &spawn_iso);
            tokens = t;
            calls = c;
        }
        if exited {
            // Reap the group even though the worker left of its own
            // accord. `exited` is about the direct child; a descendant it
            // spawned still holds the inherited stdout and stderr, and the
            // joins below read to EOF. This path has already left
            // `max_wall_s` behind, so nothing would interrupt that wait.
            // No-op when the group is already empty, which is the norm.
            kill_tree(&mut proc);
            break;
        }
        if tokens >= cap_tokens {
            killed = Some("cap".to_owned());
            kill_tree(&mut proc);
            break;
        }
        if session.is_none() && t0.elapsed().as_secs() >= cfg.session_grace_s {
            tracing::warn!(
                "harness: no session ledger under {} after {}s — \
                 killing worker rather than running it unmetered",
                session_dir.display(),
                cfg.session_grace_s
            );
            killed = Some("unmetered".to_owned());
            kill_tree(&mut proc);
            break;
        }
        if t0.elapsed().as_secs() as i64 >= max_wall_s {
            killed = Some("wallclock".to_owned());
            kill_tree(&mut proc);
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(cfg.poll_s));
    }

    let exit_code = proc.wait().ok().and_then(|s| s.code());

    // Final ledger read after exit.
    if let Some(ref sess) = session {
        let (t, c) = ledger_usage(sess, &spawn_iso);
        tokens = t;
        calls = c;
    }

    // Exited before the grace period with no ledger: `tokens_new = 0` here
    // means "unknown", never "free". Recording it Done would assert the
    // worker cost nothing, which also drags the anticipated_tokens
    // percentiles down. An existing reason (cap, wallclock) already marks
    // the run as not-Done and is more specific, so it wins.
    if session.is_none() {
        if killed.is_none() {
            killed = Some("unmetered".to_owned());
        }
        tracing::warn!(
            "harness: worker exited after {:.1}s with no session ledger under {} — \
             token spend unmetered",
            t0.elapsed().as_secs_f64(),
            session_dir.display()
        );
    }

    // Capture output tail. Every path above reaps the process group, so
    // no writer survives to hold a pipe open and the joins are short. That
    // is a precondition of this code, not an observation about it: drop
    // the reap on any one path and these joins become unbounded.
    let mut bytes = so.join().unwrap_or_default();
    bytes.extend_from_slice(&se.join().unwrap_or_default());
    let text = String::from_utf8_lossy(&bytes);
    let stdout_tail = tail_str(&text).to_owned();

    RunResult {
        exit_code,
        killed_reason: killed,
        tokens_new: tokens,
        calls,
        session_file: session.map(|p| p.to_string_lossy().into_owned()),
        duration_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        stdout_tail,
        usage_delta: None, // set by facade.rs run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_to_iso_known_dates() {
        // 1970-01-01T00:00:00.000Z
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00.000Z");
        // 2026-01-01T00:00:00.000Z = 1767225600
        assert_eq!(epoch_to_iso(1_767_225_600), "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn test_tail_str_snaps_to_char_boundary() {
        // 3-byte chars: the raw byte cut at len-TAIL_BYTES lands mid-char
        // (3000 - 2000 = 1000, and 1000 % 3 != 0), which would panic.
        let s = "€".repeat(1000);
        let tail = tail_str(&s);
        assert_eq!(tail.len(), 1998);
        assert!(s.ends_with(tail));
        assert!(tail.chars().all(|c| c == '€'));
    }

    #[test]
    fn test_tail_str_keeps_short_input_whole() {
        assert_eq!(tail_str("héllo"), "héllo");
    }

    #[test]
    fn test_drain_tail_retains_only_the_tail() {
        let mut src = vec![b'a'; TAIL_BYTES * 3];
        src.extend_from_slice(b"END");
        let out = drain_tail(Some(std::io::Cursor::new(src)))
            .join()
            .unwrap_or_default();
        assert_eq!(out.len(), TAIL_BYTES);
        assert!(out.ends_with(b"END"));
    }
}
