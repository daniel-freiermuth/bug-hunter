//! Worker runner — spawns headless omp, meters its ledger, kills at cap.
//!
//! Port of `hunter/backends/omp_scavenge/harness.py`. The cap design (exp1b):
//! never trust harness cooperation. The worker's session JSONL is written
//! live under a private per-run directory; we watch it and SIGTERM the
//! process group at the token threshold.
//!
//! Every run gets its own `--session-dir`. That is not tidiness: omp's
//! `autoResume` setting (global config, on by default for this operator)
//! makes a bare `omp -p` continue the newest session for the same cwd
//! whenever no session flag or session directory is passed. A worker that
//! resumes re-caches the whole prior transcript on its first call — the
//! observed cost was 508 709 `cacheWrite` tokens on call #1 for a repo
//! whose session had accumulated 290 calls since 2026-09-06, which trips
//! any cap before the worker does a single useful thing. Passing an
//! explicit, empty session directory is what makes each job start cold.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::types::RunResult;
use crate::util::kill_tree;

// ---------------------------------------------------------------------------
// Sessions directory
// ---------------------------------------------------------------------------

/// Resolve the omp sessions directory: `$OMP_HOME/agent/sessions` or
/// `~/.omp/agent/sessions`.
pub fn omp_sessions_dir() -> PathBuf {
    if let Ok(home) = std::env::var("OMP_HOME") {
        return PathBuf::from(home).join("agent/sessions");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home).join(".omp/agent/sessions")
}

/// Private session directory for one worker run, under the omp sessions
/// tree so operators find worker transcripts where every other omp session
/// lives. The cwd slug is omp's own naming (`/` → `-`), suffixed with the
/// spawn instant and a process-local counter so two runs in the same
/// worktree — retries of the same job — never share a directory and so
/// never resume one another.
fn run_session_dir(cwd: &Path, spawn_ms: u128) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let slug = cwd.to_string_lossy().replace('/', "-");
    let slug = slug.trim_matches('-').to_owned();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    omp_sessions_dir().join(format!("{slug}--{spawn_ms}-{seq}"))
}

// ---------------------------------------------------------------------------
// Ledger metering (harness.py:27-55)
// ---------------------------------------------------------------------------

/// Sum "new" tokens (input + output + cacheWrite) and call count from a
/// session JSONL file.
///
/// IO errors → (0, 0).
pub fn ledger_usage(session_file: &Path) -> (i64, i64) {
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

/// Meter a worker's whole private session directory: every `*.jsonl` omp
/// wrote for this run, summed. One file is the norm; summing rather than
/// picking one means an extra transcript (a nested session) is counted as
/// spend instead of silently discounted.
///
/// Returns `None` until the directory holds at least one ledger — that is
/// the "not metered yet" signal `run_worker` waits on, and kills on.
fn ledger_dir_usage(run_dir: &Path) -> Option<(PathBuf, i64, i64)> {
    let mut files: Vec<PathBuf> = fs::read_dir(run_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    if files.is_empty() {
        return None;
    }
    // omp names sessions by creation instant, so sorting by name puts the
    // run's first — the one worth recording as `session_file`.
    files.sort();
    let (mut tokens, mut calls) = (0, 0);
    for f in &files {
        let (t, c) = ledger_usage(f);
        tokens += t;
        calls += c;
    }
    Some((files.swap_remove(0), tokens, calls))
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
#[allow(
    clippy::too_many_lines,
    reason = "spawn, meter and reap are one state machine over a single \
              child process; splitting it would hand the pid, the ledger \
              cursor and the deadline across function boundaries where a \
              missed step leaks a worker"
)]
pub fn run_worker(
    cfg: &Config,
    cwd: &Path,
    prompt: &str,
    cap_tokens: i64,
    max_wall_s: i64,
    model: Option<&str>,
) -> RunResult {
    let t0 = Instant::now();
    let spawn_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let run_dir = run_session_dir(cwd, spawn_ms);
    if let Err(e) = fs::create_dir_all(&run_dir) {
        // Without a private directory the worker would fall back to omp's
        // cwd-keyed session and resume it. Refusing is cheaper than the
        // half-megatoken first call that follows.
        return RunResult {
            exit_code: Some(127),
            killed_reason: Some("unmetered".to_owned()),
            tokens_new: 0,
            calls: 0,
            session_file: None,
            duration_s: 0.0,
            stdout_tail: format!("session dir {} unusable: {e}", run_dir.display()),
            usage_delta: None,
        };
    }

    // Build command: [omp_bin, "-p", prompt] + optional flags
    let mut cmd = Command::new(&cfg.omp_bin);
    cmd.arg("-p").arg(prompt);
    cmd.arg(format!("--session-dir={}", run_dir.display()));
    if let Some(m) = model {
        cmd.arg(format!("--model={m}"));
    }
    if let Some(smol) = &cfg.model_smol {
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
        if let Some((path, t, c)) = ledger_dir_usage(&run_dir) {
            session = Some(path);
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
                run_dir.display(),
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
    if let Some((path, t, c)) = ledger_dir_usage(&run_dir) {
        session = Some(path);
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
            run_dir.display()
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
