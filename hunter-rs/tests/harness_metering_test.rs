#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_worker` end to end — metering, the unmetered verdict, output drain.
//!
//! `harness_test.rs` covers the pieces (`ledger_usage`, `snapshot`,
//! `discover`, `kill_tree`) in isolation. Nothing covered the function that
//! wires them together, which is where the accounting invariant lives:
//!
//! > a run whose token spend could not be measured must never be recorded
//! > as costing zero.
//!
//! `tokens_new = 0` means "unknown", never "free". A review round found
//! `run_worker` dropping a ledger-less worker through as a clean success:
//! the budget then under-counts the window *and* the false zero drags the
//! `anticipated_tokens` percentiles down, so the next job is sized against
//! a spend that never happened. `killed_reason` is the flag that stops it —
//! `scheduler::job_state` maps `Some(_)` to `JobState::Killed`, and only
//! `None` + exit 0 to `JobState::Done`.
//!
//! The seams: `cfg.omp_bin` is looked up on `PATH` (`FakeBins`), and
//! `omp_sessions_dir()` reads `$OMP_HOME` from the daemon's own
//! environment (`OmpHome`). Without the second one these tests would read
//! the developer's real `~/.omp/agent/sessions`, where a live omp session
//! is writing.

mod support;

use std::path::PathBuf;

use hunter::backends::omp_scavenge::harness;
use hunter::config::Config;
use hunter::types::RunResult;
use support::{FakeBins, TempDir};

const PROMPT: &str = "hunt the bug";

/// Pipe capacity on Linux is 64 KiB; a worker writing more than that
/// blocks until someone reads, which is what `drain_tail` exists to do.
const PIPE_BUFFER: usize = 64 * 1024;

/// `run_worker` keeps this many bytes of worker output.
const TAIL_BYTES: usize = 2000;

/// Characters per `emit_sh` chunk.
const CHUNK_CHARS: usize = 1024;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A scripted `omp` on `PATH`, an empty sessions tree, and a worktree.
///
/// Field order is drop order: `OMP_HOME` is restored first, then `PATH`
/// and the process-wide lock, then the scratch directory goes away.
struct Fixture {
    _home: support::EnvGuard,
    bins: FakeBins,
    _dir: TempDir,
    sessions: PathBuf,
    cwd: PathBuf,
    cfg: Config,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let bins = FakeBins::acquire(label); // take the lock before touching env
        let dir = TempDir::new(label);
        let omp_home = dir.subdir("omp-home");
        let sessions = omp_home.join("agent/sessions");
        std::fs::create_dir_all(&sessions).expect("create sessions dir");
        let cwd = dir.subdir("work");
        // Scoped through FakeBins: it owns the process-env lock, so taking
        // one is what licenses mutating the environment at all.
        let home = bins.env("OMP_HOME", &omp_home);

        // No config.json under a scratch root → every default, including
        // `ompBin = "omp"`. Only the meter tick is overridden: the 2 s
        // production poll would make each of these tests a 2 s test.
        let mut cfg = Config::load(dir.path()).expect("load config");
        cfg.poll_s = 0.02;

        Self {
            _home: home,
            bins,
            _dir: dir,
            sessions,
            cwd,
            cfg,
        }
    }

    /// omp's own naming: the worker's cwd with `/` turned into `-`. Using
    /// the real slug exercises `discover`'s fuzzy match rather than its
    /// most-recently-modified fallback.
    fn slug(&self) -> String {
        self.cwd
            .to_string_lossy()
            .replace('/', "-")
            .trim_matches('-')
            .to_owned()
    }

    /// Shell that writes an OMP-style session ledger into the private
    /// `--session-dir` supplied by the harness.
    fn ledger_sh(&self, records: &[(i64, i64, i64)]) -> String {
        let mut sh = concat!(
            "session_dir=''\n",
            "for arg in \"$@\"; do\n",
            "  case \"$arg\" in --session-dir=*) session_dir=${arg#--session-dir=} ;; esac\n",
            "done\n",
            "test -n \"$session_dir\" || exit 90\n",
            "mkdir -p \"$session_dir\"\n",
            "file=\"$session_dir/session-01.jsonl\"\n",
            "TS=$(date -u '+%Y-%m-%dT%H:%M:%S.000Z')\n",
        )
        .to_owned();
        for &(input, output, cache_write) in records {
            let record = serde_json::json!({
                "timestamp": "@TS@",
                "message": {
                    "role": "assistant",
                    "usage": {
                        "input": input,
                        "output": output,
                        "cacheWrite": cache_write,
                    },
                },
            })
            .to_string()
            .replace("@TS@", "%s");
            sh.push_str(&format!("printf '{record}\\n' \"$TS\" >> \"$file\"\n"));
        }
        sh
    }

    fn run(&self, cap_tokens: i64, max_wall_s: i64) -> RunResult {
        harness::run_worker(&self.cfg, &self.cwd, PROMPT, cap_tokens, max_wall_s, None)
    }
}

/// Shell emitting `chunks` repeats of `CHUNK_CHARS` copies of `ch` on
/// stdout (`fd` 1) or stderr (`fd` 2). A shell loop over `printf`, so
/// nothing depends on `dd` or a `head` that understands `-c`.
fn emit_sh(ch: char, chunks: usize, fd: u8) -> String {
    let chunk: String = std::iter::repeat_n(ch, CHUNK_CHARS).collect();
    let redirect = if fd == 2 { " >&2" } else { "" };
    format!(
        "i=0\nwhile [ $i -lt {chunks} ]; do printf '%s' '{chunk}'{redirect}; i=$((i+1)); done\n"
    )
}

// ---------------------------------------------------------------------------
// Metering
// ---------------------------------------------------------------------------

/// The happy path, so every "was flagged" test below is known to be
/// asserting on a difference rather than on a harness that always flags.
#[test]
fn metered_worker_sums_the_ledger_and_is_not_flagged() {
    let fx = Fixture::new("metered");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}printf 'worker-done\\n'\nexit 0",
            ledger = fx.ledger_sh(&[(1000, 200, 50), (3000, 400, 350)])
        ),
    );

    let res = harness::run_worker(&fx.cfg, &fx.cwd, PROMPT, 1_000_000, 30, Some("test-model"));

    assert_eq!(res.killed_reason, None, "a metered run carries no flag");
    assert_eq!(res.exit_code, Some(0));
    assert_eq!(res.tokens_new, 5000, "(1000+200+50) + (3000+400+350)");
    assert_eq!(res.calls, 2);
    assert!(
        res.session_file
            .as_deref()
            .is_some_and(|f| f.ends_with("session-01.jsonl")),
        "ledger path recorded, got {:?}",
        res.session_file
    );
    assert!(res.stdout_tail.contains("worker-done"));

    let calls = fx.bins.calls_to("omp");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][..3], ["omp", "-p", PROMPT]);
    assert!(
        calls[0].iter().any(|arg| arg.starts_with("--session-dir=")),
        "worker must receive an exclusive session directory: {:?}",
        calls[0]
    );
    assert!(calls[0].contains(&"--model=test-model".to_owned()));
}

/// The regression a review round found: the worker exited 0, promptly, and
/// left no ledger. Zero tokens here is "unknown", and recording it as a
/// clean success both under-counts the window and feeds a false zero into
/// the `anticipated_tokens` percentiles.
#[test]
fn worker_without_a_ledger_is_flagged_unmetered_despite_exit_zero() {
    let fx = Fixture::new("unmetered");
    fx.bins.script("omp", "printf 'nothing metered\\n'\nexit 0");

    let res = fx.run(1_000_000, 30);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("unmetered"),
        "a run nobody could meter is not a clean success"
    );
    // Exit 0 with no flag is the only shape `scheduler::job_state` calls
    // Done; the flag above is what keeps this run out of that bucket.
    assert_eq!(res.exit_code, Some(0), "the worker really did exit cleanly");
    assert_eq!(res.tokens_new, 0);
    assert_eq!(res.calls, 0);
    assert_eq!(res.session_file, None);
}

/// A shared default-session ledger, even for the same checkout, must never
/// be attributed to this worker. Only its freshly allocated
/// `--session-dir` is eligible for metering.
#[test]
fn stale_ledger_from_an_earlier_run_is_not_counted() {
    let fx = Fixture::new("stale-ledger");
    let dir = fx.sessions.join(fx.slug());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session-01.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": "2999-01-01T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "usage": {"input": 90_000, "output": 1, "cacheWrite": 1},
                },
            })
        ),
    )
    .unwrap();
    fx.bins.script("omp", "exit 0");

    let res = fx.run(1_000_000, 30);

    assert_eq!(res.tokens_new, 0, "another run's ledger is not our spend");
    assert_eq!(res.session_file, None);
    assert_eq!(res.killed_reason.as_deref(), Some("unmetered"));
}

// ---------------------------------------------------------------------------
// Kill reasons
// ---------------------------------------------------------------------------

/// "unmetered" is the weakest of the three reasons: it only says the spend
/// is unknown. `wallclock` says *why* the worker died and is what the
/// operator needs to see, so the later unmetered check must not overwrite
/// it — and this is the one combination where both fire, since a `cap`
/// kill can only happen once a ledger has been found.
#[test]
fn an_existing_kill_reason_survives_the_unmetered_check() {
    let fx = Fixture::new("wallclock-wins");
    fx.bins.script("omp", "sleep 10");

    let res = fx.run(1_000_000, 1);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("wallclock"),
        "the specific reason must not be clobbered by the generic one"
    );
    assert_eq!(
        res.session_file, None,
        "no ledger — so the unmetered branch really was reached"
    );
    assert!(
        res.duration_s < 5.0,
        "killed at the wall limit, not after the worker's own sleep: {}s",
        res.duration_s
    );
}

/// The cap watchdog, end to end: the ledger crosses the threshold while
/// the worker is still running and the process group is killed.
#[test]
fn cap_kill_fires_while_the_worker_is_still_running() {
    let fx = Fixture::new("cap-kill");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}sleep 10",
            ledger = fx.ledger_sh(&[(400_000, 50_000, 50_000)])
        ),
    );

    let res = fx.run(1_000, 30);

    assert_eq!(res.killed_reason.as_deref(), Some("cap"));
    assert_eq!(res.tokens_new, 500_000, "metered before the kill");
    assert_ne!(res.exit_code, Some(0), "a capped worker did not succeed");
    assert!(
        res.duration_s < 5.0,
        "killed on crossing the cap, not after the worker's own sleep: {}s",
        res.duration_s
    );
}

/// A missing `omp` is a failure, not a free run: exit 127 keeps it out of
/// `JobState::Done` even though `killed_reason` is None and no tokens were
/// counted.
#[test]
fn missing_omp_binary_is_a_failure_not_a_free_run() {
    let fx = Fixture::new("no-omp");
    // Nothing scripted, and the real PATH dropped: otherwise a developer
    // with omp installed spawns an actual worker from this test.
    fx.bins.isolate();

    let res = fx.run(1_000_000, 30);

    assert_eq!(res.exit_code, Some(127));
    assert_ne!(res.exit_code, Some(0), "never a clean success");
    assert_eq!(res.tokens_new, 0);
    assert!(
        res.stdout_tail.contains("spawn error"),
        "got {:?}",
        res.stdout_tail
    );
}

// ---------------------------------------------------------------------------
// Output capture
// ---------------------------------------------------------------------------

/// Both pipes have to be drained *while* the worker runs. A worker that
/// writes past the 64 KiB pipe buffer blocks in `write()` until someone
/// reads, and a blocked worker never exits and never writes another ledger
/// line — the cap watchdog goes blind and the run dies on the wall limit.
///
/// The script fills stdout first and only then writes stderr, so seeing
/// the stderr marker proves the stdout pipe was being drained too.
#[test]
fn oversized_output_on_both_pipes_does_not_deadlock() {
    let fx = Fixture::new("big-output");
    let chunks = PIPE_BUFFER * 2 / CHUNK_CHARS;
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}{out}printf 'STDOUT-END\\n'\n{err}printf 'STDERR-END\\n' >&2\nexit 0",
            ledger = fx.ledger_sh(&[(4000, 200, 42)]),
            out = emit_sh('a', chunks, 1),
            err = emit_sh('b', chunks, 2),
        ),
    );

    let res = fx.run(1_000_000, 5);

    assert_eq!(
        res.killed_reason, None,
        "a deadlocked worker dies on the wall limit instead"
    );
    assert_eq!(res.exit_code, Some(0));
    assert_eq!(res.tokens_new, 4242, "still metered through all that noise");
    assert!(
        res.stdout_tail.ends_with("STDERR-END\n"),
        "both pipes drained to EOF; tail ended {:?}",
        &res.stdout_tail[res.stdout_tail.len().saturating_sub(40)..]
    );
    assert!(
        res.stdout_tail.len() <= TAIL_BYTES,
        "{} bytes kept of a {}-byte worker",
        res.stdout_tail.len(),
        chunks * CHUNK_CHARS * 2
    );
}

/// A worker that emits bytes that are not UTF-8 (a truncated build log, a
/// binary blob echoed by mistake) still has usable output: the bad bytes
/// become replacement characters and everything around them survives.
/// Dropping the capture would throw away the only diagnostic a killed run
/// leaves behind.
#[test]
fn invalid_utf8_output_is_kept_lossily() {
    let fx = Fixture::new("bad-utf8");
    fx.bins.script(
        "omp",
        "printf 'START-'\nprintf '\\377\\376\\375\\300'\nprintf -- '-END\\n'\nexit 0",
    );

    let res = fx.run(1_000_000, 30);

    assert_eq!(res.exit_code, Some(0));
    assert!(
        res.stdout_tail.starts_with("START-"),
        "output before the bad bytes survives: {:?}",
        res.stdout_tail
    );
    assert!(
        res.stdout_tail.ends_with("-END\n"),
        "output after the bad bytes survives: {:?}",
        res.stdout_tail
    );
    assert!(
        res.stdout_tail.contains('\u{FFFD}'),
        "bad bytes replaced, not dropped: {:?}",
        res.stdout_tail
    );
}

/// The tail is a byte window over a byte stream, so its left edge lands
/// mid-character whenever the worker speaks anything but ASCII — here the
/// 2000-byte cut falls inside a 3-byte `€`. Slicing there panics, and a
/// panic in the harness loses the whole run, not just its output.
#[test]
fn output_cut_mid_multibyte_char_is_not_a_panic() {
    let fx = Fixture::new("mid-char");
    // One chunk of '€' is 3072 bytes, so the 2000-byte window cuts into
    // it — and 2000 - "END\n" = 1996 is not a multiple of 3, so the cut
    // lands on a continuation byte rather than a character start.
    fx.bins.script(
        "omp",
        &format!(
            "{euros}printf 'END\\n'\nexit 0",
            euros = emit_sh('\u{20AC}', 1, 1)
        ),
    );

    let res = fx.run(1_000_000, 30);

    assert_eq!(res.exit_code, Some(0));
    assert!(res.stdout_tail.ends_with("END\n"));
    assert!(
        res.stdout_tail.len() <= TAIL_BYTES,
        "kept {} bytes",
        res.stdout_tail.len()
    );
    assert!(
        res.stdout_tail
            .trim_end_matches("END\n")
            .chars()
            .all(|c| c == '€'),
        "the boundary snap must not mangle the characters it keeps: {:?}",
        res.stdout_tail.chars().take(8).collect::<String>()
    );
}

/// The sibling of `worker_without_a_ledger_is_flagged_unmetered_despite_exit_zero`:
/// a worker that never exits and never produces a ledger.
///
/// This is the branch the cap exists for — a worker burning tokens that
/// nothing can measure. It was untestable while the grace period was a
/// hardcoded 120 s constant; `sessionGraceS` makes it injectable, exactly
/// as `maxWallS` already is for the wall-clock branch in the same loop.
/// Shortening the interval is the only honest option here: the sleep is
/// `std::thread::sleep` on a blocking thread, so `tokio::time::pause()`
/// has no effect on it.
#[test]
fn hanging_worker_without_a_ledger_is_killed_as_unmetered() {
    let mut fx = Fixture::new("unmetered-hang");
    fx.cfg.session_grace_s = 1;
    // Sleeps well past the grace period and writes no ledger.
    fx.bins.script("omp", "sleep 30");

    let started = std::time::Instant::now();
    // max_wall_s is far larger, so a pass cannot be the wall-clock branch.
    let res = fx.run(1_000_000, 300);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("unmetered"),
        "a worker that never produced a ledger must be killed, not left running"
    );
    assert_eq!(res.tokens_new, 0);
    assert!(
        started.elapsed().as_secs() < 20,
        "must be killed at the grace period, not left to run: {:?}",
        started.elapsed()
    );
}

/// A worker that exits while leaving a descendant behind must not hang the
/// harness.
///
/// This is the normal-exit path: the loop breaks the moment `try_wait`
/// reports `omp` gone, so no deadline is in force any more — `max_wall_s`
/// has already been left behind. The reader threads drain to EOF, and a
/// descendant that inherited stdout and stderr holds those pipes open, so
/// the join never returns. `omp` spawning subagents, tools and git makes
/// this the most likely place in the daemon to meet one.
#[test]
fn worker_leaving_a_descendant_behind_does_not_hang_the_harness() {
    let fx = Fixture::new("lingering");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}sleep 30 &\nprintf 'worker-done\\n'\nexit 0",
            ledger = fx.ledger_sh(&[(1000, 200, 50)])
        ),
    );

    let t0 = std::time::Instant::now();
    let res = harness::run_worker(&fx.cfg, &fx.cwd, PROMPT, 1_000_000, 30, None);
    let elapsed = t0.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "run_worker blocked on a descendant holding the pipes: {elapsed:?}"
    );
    assert_eq!(res.exit_code, Some(0), "the worker itself exited cleanly");
    assert!(
        res.stdout_tail.contains("worker-done"),
        "output captured before the group was reaped: {:?}",
        res.stdout_tail
    );
}
