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
//!
//! The exception is a deliberate continuation: `run_worker` can be handed
//! the session file of a job that ran out of window headroom mid-flight
//! and continue *that* transcript by naming its path. That is a different
//! mechanism from `autoResume` and the two must not be confused — see the
//! `--resume` argument in `run_worker` for what was measured.

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

/// How many worker transcripts to keep under `<work_root>/sessions`.
///
/// One directory per run, never reused, so without a bound this grows for
/// the life of the deployment — at the observed job rate a few thousand
/// transcripts a year, each tens of megabytes. Keeping the most recent N
/// leaves enough to debug a failure that was noticed days later, which is
/// what these are read for once metering is done with them.
const SESSIONS_RETAINED: usize = 50;

/// Root for per-run session directories: `<work_root>/sessions`.
///
/// Hunter's own data, under hunter's own work root — NOT the operator's
/// `~/.omp/agent/sessions`. A directory per run in the shared tree grows
/// without bound inside a directory a human also uses, and nothing else
/// prunes it. Being easy to find is served just as well by a documented
/// path we control.
pub fn sessions_root(work_root: &Path) -> PathBuf {
    work_root.join("sessions")
}

/// Private session directory for one worker run. The cwd slug is omp's own
/// naming (`/` → `-`), suffixed with the spawn instant and a process-local
/// counter so two runs in the same worktree — retries of the same job —
/// never share a directory and so never resume one another.
fn run_session_dir(work_root: &Path, cwd: &Path, spawn_ms: u128) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let slug = cwd.to_string_lossy().replace('/', "-");
    let slug = slug.trim_matches('-');
    // The whole path collapses into ONE component, and a component is
    // capped at NAME_MAX (255 bytes on Linux) -- so a deep enough
    // `workRoot` makes `create_dir_all` fail with ENAMETOOLONG before
    // omp is ever spawned, and the worker comes back
    // `killed_reason: "unmetered"`: a path length reported as a budget
    // failure. Keep the TAIL, which is the end that distinguishes one
    // worktree from another (`.../worktrees/f{fid}`); a head-truncated
    // slug would collide across findings. The suffix adds ~30 bytes.
    let slug = crate::util::tail(slug, 150).to_owned();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    sessions_root(work_root).join(format!("{slug}--{spawn_ms}-{seq}"))
}

/// Drop all but the newest [`SESSIONS_RETAINED`] run directories.
///
/// Called before a run creates its own, so the live directory is never a
/// candidate. Best effort throughout: a transcript that cannot be removed
/// is a disk-space problem, not a reason to refuse the job that was about
/// to start.
pub fn prune_sessions(work_root: &Path) -> usize {
    let root = sessions_root(work_root);
    let Ok(entries) = fs::read_dir(&root) else {
        return 0;
    };
    let mut dirs: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if dirs.len() <= SESSIONS_RETAINED {
        return 0;
    }
    // Newest first, then drop the tail.
    dirs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let mut removed = 0;
    for (_, path) in dirs.drain(SESSIONS_RETAINED..) {
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

// ---------------------------------------------------------------------------
// Ledger metering (`harness.ledger_usage`, `harness._ledger_dir_usage`)
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

/// The context a session had reached when it stopped: the LAST usage
/// record's `input + cacheRead + cacheWrite`.
///
/// Same file and same records [`ledger_usage`] walks, different
/// question. `ledger_usage` sums what a run ADDED, deliberately
/// excluding `cacheRead` because a cache hit is not new spend. Resuming
/// asks the opposite: the first call of a resumed session re-establishes
/// the whole context, so what it will cost is the context size at the
/// moment of suspension — cache reads included. Measured across 112
/// production re-cache events the re-cache / prior-context ratio had
/// median 1.00 and p10 1.00, so the last record's total is the estimate,
/// not a lower bound to pad.
///
/// The LAST record rather than the largest: context grows monotonically
/// within a session, and the final call is the one the resumed session
/// picks up from.
///
/// `None` when the file is unreadable or holds no usage record at all —
/// the caller has a documented fallback, and inventing a number here
/// would hide a missing transcript inside a plausible-looking estimate.
pub fn ctx_at_suspension(session_file: &Path) -> Option<i64> {
    let file = fs::File::open(session_file).ok()?;
    let reader = BufReader::new(file);
    let mut last: Option<i64> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue; // partial trailing line mid-write
        };
        let Some(msg) = rec.get("message").and_then(|v| v.as_object()) else {
            continue;
        };
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(u) = msg.get("usage").and_then(|v| v.as_object()) {
            let field = |name: &str| u.get(name).and_then(serde_json::Value::as_i64).unwrap_or(0);
            last = Some(field("input") + field("cacheRead") + field("cacheWrite"));
        }
    }
    last
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

/// First line of a file, or empty on any IO error.
///
/// Only ever compared against itself: omp writes a `{"type":"title"…}`
/// header as a session's opening record and never rewrites it, so that
/// line is a cheap identity for "still the same session".
fn first_line(path: &Path) -> Vec<u8> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut line = Vec::new();
    let _ = BufReader::new(file).read_until(b'\n', &mut line);
    line
}

/// A resume that cannot happen, reported rather than silently downgraded.
///
/// The caller asked to continue one specific transcript. If that
/// transcript is gone, spawning a cold worker here would be
/// indistinguishable from a successful resume anywhere downstream — same
/// exit code, same session path — while paying a fresh session floor and
/// redoing the work the resume existed to avoid. Whether to start clean
/// is the scheduler's decision, so the scheduler is the one told.
fn resume_unavailable(session_file: &Path, why: &str) -> RunResult {
    tracing::warn!("harness: cannot resume {}: {why}", session_file.display());
    RunResult {
        exit_code: None,
        killed_reason: Some("resume-unavailable".to_owned()),
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 0.0,
        stdout_tail: format!("cannot resume {}: {why}", session_file.display()),
        usage_delta: None,
    }
}

// ---------------------------------------------------------------------------
// Worker execution (`harness.run_worker`)
// ---------------------------------------------------------------------------

/// Spawn `omp -p`, meter its JSONL session ledger, kill at the cap.
///
/// `cap_tokens` is the ramp's headroom for this run. `None` is a run with
/// no token bound — the ramp found no ceiling to impose — and `max_wall_s`
/// is then the only thing that stops a worker that will not stop itself.
///
/// `resume_from` is the session file of an earlier attempt to continue.
/// `None` starts cold in a private directory, which is what every job did
/// before resume existed. `Some` continues that transcript in place and
/// meters only what this attempt adds to it; a source that is no longer
/// on disk is refused outright (`killed_reason = "resume-unavailable"`)
/// rather than quietly downgraded to a cold run.
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
    cap_tokens: Option<i64>,
    max_wall_s: i64,
    model: Option<&str>,
    resume_from: Option<&Path>,
) -> RunResult {
    let t0 = Instant::now();
    let spawn_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // A resume continues one named transcript, so this attempt's run
    // directory is the predecessor's rather than a new one: omp writes the
    // continued session at the `--resume` path and `--session-dir` does
    // not move it (see the argument below).
    let mut resume_head: Option<Vec<u8>> = None;
    let run_dir = if let Some(sf) = resume_from {
        let Some(dir) = sf.parent().filter(|d| d.is_dir()) else {
            return resume_unavailable(sf, "session directory is gone");
        };
        // Empty counts as gone: omp treats an unreadable resume source
        // as "start fresh here", which is the failure this refuses.
        if fs::metadata(sf).map_or(0, |m| m.len()) == 0 {
            return resume_unavailable(sf, "session file is missing or empty");
        }
        resume_head = Some(first_line(sf));
        dir.to_owned()
    } else {
        // Pruning is only safe for a directory this run is about to
        // create: it keeps the newest N by mtime, and a transcript
        // worth resuming is old by construction.
        prune_sessions(&cfg.work_root);
        run_session_dir(&cfg.work_root, cwd, spawn_ms)
    };
    // Attribution baseline. A resumed worker appends to the predecessor's
    // ledger and `ledger_dir_usage` sums the whole directory, so every
    // token already in it was paid for by the predecessor's job row.
    // Subtracting the pre-spawn totals is what keeps this attempt's
    // `tokens_new` this attempt's — without it a resumed chain re-bills
    // its own history on every link.
    let (base_tokens, base_calls) = if resume_from.is_some() {
        ledger_dir_usage(&run_dir).map_or((0, 0), |(_, t, c)| (t, c))
    } else {
        (0, 0)
    };
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
    if let Some(sf) = resume_from {
        // Measured against omp v18.2.6 on 2026-09-24, because the obvious
        // reading of these two flags is that they conflict and they do
        // not: `--resume=<path>` and `--session-dir=<dir>` coexist, and
        // under `-p` the named session continues non-interactively. Proof
        // from the probe: the resumed run's transcript kept the first
        // exchange byte for byte and chained its new records onto the old
        // file's last one, while the same prompt in a fresh session
        // directory answered "there is no earlier reply in this
        // conversation". This is emphatically not omp's `autoResume`,
        // which continues "the newest session for this cwd" — naming the
        // path is what makes the continuation the one we meant.
        //
        // `--resume` also decides where the transcript is written: omp
        // appends to the named file and `--session-dir` does not override
        // that. An unresolvable path is not an error — omp starts a fresh
        // session, writes it at that path, and exits 0 with no diagnostic
        // — which is why the source is checked before this spawn instead
        // of trusted after it.
        //
        // Cost shape, same probe: the cold call wrote 22 976 `cacheWrite`
        // and read 0; the resumed call wrote 28 and read 22 976. Inside
        // the prompt-cache TTL a resume re-caches essentially nothing.
        // Past it the re-cache costs the context size at suspension (112
        // production re-cache events, median ratio 1.00) — still bounded
        // by the transcript, never by redoing the work that produced it.
        cmd.arg(format!("--resume={}", sf.display()));
    }
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
    // Explicitly null, never inherited: omp reads a piped stdin as extra
    // prompt text and blocks on EOF before it initialises the session --
    // "Reading prompt from piped stdin (waiting for EOF)". A worker that
    // does that writes no ledger at all, so it is killed as `unmetered`
    // after the grace period, having spent its whole wall-clock slot
    // doing nothing.
    //
    // Today the daemon happens to have /dev/null on fd 0, because the
    // unit sets no StandardInput and systemd's default is null. That is
    // an inherited accident, not a decision: run the daemon from a
    // supervisor or a wrapper that pipes stdin and every worker hangs.
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
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
            // Clamped because the subtraction has one way to go negative:
            // the transcript shrinking, i.e. being replaced rather than
            // appended to. A negative running total would sit below every
            // cap and disarm the watchdog outright.
            tokens = (t - base_tokens).max(0);
            calls = (c - base_calls).max(0);
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
        if cap_tokens.is_some_and(|cap| tokens >= cap) {
            killed = Some("cap".to_owned());
            kill_tree(&mut proc);
            break;
        }
        // On a resume the directory already holds the predecessor's
        // transcript, so "a ledger exists" says nothing about this
        // attempt; the equivalent signal is "this attempt has recorded no
        // call yet".
        let unmetered = if resume_from.is_some() {
            calls == 0
        } else {
            session.is_none()
        };
        if unmetered && t0.elapsed().as_secs() >= cfg.session_grace_s {
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

    // Did the resume take? omp answers in the one place it costs nothing
    // to look: a fresh session written at the `--resume` path replaces the
    // file's opening record, where a real continuation appends and leaves
    // it untouched. Nothing in the exit code distinguishes the two, and
    // missing it means believing work was continued that was in fact
    // redone from scratch at full price.
    let mut resume_lost = false;
    if let (Some(sf), Some(head)) = (resume_from, &resume_head) {
        resume_lost = first_line(sf) != *head;
        if resume_lost {
            tracing::warn!(
                "harness: --resume {} did not continue that session — omp \
                 started a fresh one at the same path, so this attempt \
                 redid the work it was meant to continue",
                sf.display()
            );
        }
    }
    // A lost resume took the predecessor's transcript with it, so the
    // baseline measured against it no longer describes anything on disk:
    // whatever records remain were all written by this attempt.
    let (base_tokens, base_calls) = if resume_lost {
        (0, 0)
    } else {
        (base_tokens, base_calls)
    };

    // Final ledger read after exit.
    if let Some((path, t, c)) = ledger_dir_usage(&run_dir) {
        session = Some(path);
        tokens = (t - base_tokens).max(0);
        calls = (c - base_calls).max(0);
    }

    // Exited before the grace period with no ledger: `tokens_new = 0` here
    // means "unknown", never "free". Recording it Done would assert the
    // worker cost nothing, which also drags the anticipated_tokens
    // percentiles down. An existing reason (cap, wallclock) already marks
    // the run as not-Done and is more specific, so it wins.
    let unmetered = if resume_from.is_some() {
        calls == 0
    } else {
        session.is_none()
    };
    if unmetered {
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
    // The marker rides on the output tail because that is what a killed
    // run leaves an operator to read; it is worth the few bytes over
    // `TAIL_BYTES` that it costs.
    let stdout_tail = if resume_lost {
        format!("[resume-lost] {}", tail_str(&text))
    } else {
        tail_str(&text).to_owned()
    };

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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod run_session_dir_tests {
    use super::*;

    /// A deep `workRoot` must still produce a creatable directory.
    ///
    /// The slug is one component and components cap at `NAME_MAX` (255
    /// on Linux), so without the bound `create_dir_all` fails with
    /// `ENAMETOOLONG` before omp starts and the worker is reported
    /// `unmetered` -- a path length surfacing as a budget failure.
    #[test]
    fn a_deep_cwd_still_yields_a_creatable_session_dir() {
        let root = std::env::temp_dir().join(format!("hunter-slug-{}", std::process::id()));
        let deep = std::path::PathBuf::from("/")
            .join("x".repeat(80))
            .join("y".repeat(80))
            .join("z".repeat(80));

        let dir = run_session_dir(&root, &deep, 1_790_000_000_000);
        let component = dir.file_name().expect("has a final component");
        assert!(
            component.len() <= 255,
            "session dir component is {} bytes, over NAME_MAX: {component:?}",
            component.len()
        );

        // The bound is only meaningful if the directory can be made.
        std::fs::create_dir_all(&dir).expect("session dir must be creatable");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Truncation keeps the tail, so two worktrees under one long root
    /// do not collapse onto the same session directory.
    #[test]
    fn two_deep_worktrees_keep_distinct_session_dirs() {
        let root = std::path::Path::new("/tmp/root");
        let base = std::path::PathBuf::from("/")
            .join("q".repeat(200))
            .join("worktrees");
        let a = run_session_dir(root, &base.join("f1"), 1);
        let b = run_session_dir(root, &base.join("f2"), 1);
        assert_ne!(a, b, "head-truncation would collide these");
    }
}
