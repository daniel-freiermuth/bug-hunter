//! Shared test support.
//!
//! Integration tests in `tests/` are separate crates, so each one `mod
//! support;`s this file. Everything here exists because it was being
//! reinvented per file, or because a behaviour we shipped a bug in was
//! untestable without it.
//!
//! The four pieces:
//! - [`TempDir`] — scratch directory that cleans up on drop, *including*
//!   when a test panics. The hand-rolled `let _ = fs::remove_dir_all(..)`
//!   at the end of a test body leaks on every failure.
//! - [`fresh_store`] / [`fresh_pool`] — a writable database from the
//!   schema-only `dev.db` that `build.rs` generates.
//! - [`FakeBins`] — scripted `gh` / `glab` / any binary on `PATH`, with an
//!   invocation log. The forge is constructed internally by the runners
//!   (`forge::forge_for(repo.forge)`), so there is no trait seam to inject:
//!   intercepting the subprocess is how you control forge behaviour, and it
//!   has the side benefit of exercising the real argv construction.
//! - [`ScriptedBackend`] — a `Backend` whose `run()` is a closure over the
//!   worktree, so a test can simulate exactly what a worker leaves behind
//!   (a `WITHDRAW.md`, a `findings.json`, nothing at all).
#![allow(unsafe_code)] // env::set_var for PATH interception; tests only
#![allow(dead_code)] // each test crate uses a different subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hunter::backend::JobClass;
use hunter::backend::{Backend, Outlook, Verdict};
use hunter::store::Store;
use hunter::types::RunResult;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

/// A scratch directory removed on drop, panic or not.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hunter-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }

    /// Create a subdirectory and return it.
    pub fn subdir(&self, rel: impl AsRef<Path>) -> PathBuf {
        let p = self.join(rel);
        std::fs::create_dir_all(&p).expect("create subdir");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Databases
// ---------------------------------------------------------------------------

/// Path to the schema-only `dev.db` that `build.rs` regenerates from
/// `migrations/`. Never opened directly — always copied first.
fn dev_db() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("dev.db")
}

/// A writable copy of `dev.db` (schema complete, zero rows).
pub fn db_copy(dir: &TempDir, name: &str) -> PathBuf {
    let dst = dir.join(format!("{name}.db"));
    std::fs::copy(dev_db(), &dst).expect("copy dev.db — run `just dev-db` if missing");
    dst
}

/// Raw pool over a fresh database copy, for seeding fixtures.
pub async fn fresh_pool(dir: &TempDir, name: &str) -> (PathBuf, SqlitePool) {
    let path = db_copy(dir, name);
    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .expect("open pool");
    (path, pool)
}

/// The read-write `Store` the daemon uses, over a fresh database copy.
pub async fn fresh_store(dir: &TempDir, name: &str) -> (PathBuf, Store) {
    let path = db_copy(dir, name);
    let store = Store::connect(&path).await.expect("connect store");
    (path, store)
}

// ---------------------------------------------------------------------------
// Fake executables on PATH
// ---------------------------------------------------------------------------

/// Fail loudly if this suite is run without per-test process isolation.
///
/// `FakeBins` mutates `PATH` and the environment, which are process-global.
/// Under `cargo nextest run` each test is its own process, so that is
/// private and needs no coordination. Under plain `cargo test` the tests
/// are threads in one process and would silently race — one test's `PATH`
/// landing in another's subprocess, intermittently, usually on CI.
///
/// This crate is nextest-only precisely so that the coordination can be
/// deleted rather than maintained. Asserting on the execution mode rather
/// than merely on `NEXTEST` checks the guarantee we actually depend on.
fn require_process_isolation() {
    assert_eq!(
        std::env::var("NEXTEST_EXECUTION_MODE").ok().as_deref(),
        Some("process-per-test"),
        "these tests mutate PATH and the environment and require per-test \
         process isolation; run them with `cargo nextest run` (plain \
         `cargo test` would race silently)"
    );
}

/// Scripted executables shadowing the real ones for the duration of a test.
///
/// The original `PATH` is appended, so binaries you do *not* script
/// (notably `git`) still resolve normally.
pub struct FakeBins {
    dir: TempDir,
    log: PathBuf,
    original_path: String,
}

impl FakeBins {
    pub fn acquire(label: &str) -> Self {
        require_process_isolation();
        let dir = TempDir::new(&format!("bin-{label}"));
        let log = dir.join("invocations.log");
        std::fs::write(&log, "").expect("create invocation log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let combined = format!("{}:{original_path}", dir.path().display());
        // SAFETY: this process runs exactly one test (asserted above), so
        // nothing else here reads or writes the environment. Restored in
        // `Drop` regardless, so the assertion is not load-bearing for
        // correctness within the test itself.
        unsafe { std::env::set_var("PATH", &combined) };
        Self {
            dir,
            log,
            original_path,
        }
    }

    /// Scope an environment variable for the lifetime of the returned
    /// guard.
    ///
    /// Hangs off `FakeBins` so the process-isolation assertion in
    /// `acquire` covers environment mutation too, not just `PATH`.
    ///
    /// Needed because some seams are only reachable through the
    /// environment — `harness::omp_sessions_dir()` reads `$OMP_HOME`, so
    /// redirecting session discovery at a `TempDir` has no other route,
    /// and without it those tests read the real `~/.omp/agent/sessions`
    /// that a live daemon is writing to.
    pub fn env(&self, key: &'static str, value: &Path) -> EnvGuard {
        let previous = std::env::var(key).ok();
        // SAFETY: one test per process; restored on drop.
        unsafe { std::env::set_var(key, value) };
        EnvGuard { key, previous }
    }

    /// Drop the real `PATH`, leaving only the scripted directory.
    ///
    /// `acquire` appends the original `PATH` so unscripted binaries (git)
    /// still resolve. Use this when the point of the test is that a binary
    /// is ABSENT — otherwise a developer's real `npx`/`gh` answers instead.
    pub fn isolate(&self) {
        // SAFETY: one test per process; restored in `Drop`.
        unsafe { std::env::set_var("PATH", self.dir.path()) };
    }

    /// Install `name` as an executable running `body` (POSIX sh). The script
    /// appends its argv to the invocation log before running `body`.
    pub fn script(&self, name: &str, body: &str) {
        let path = self.dir.join(name);
        let contents = format!(
            "#!/bin/sh\nprintf '%s' \"{name}\" >> '{log}'\nfor a in \"$@\"; do printf '\\t%s' \"$a\" >> '{log}'; done\nprintf '\\n' >> '{log}'\n{body}\n",
            name = name,
            log = self.log.display(),
        );
        std::fs::write(&path, contents).expect("write fake binary");
        set_executable(&path);
    }

    /// Succeed, printing `stdout`.
    pub fn ok(&self, name: &str, stdout: &str) {
        self.script(
            name,
            &format!("cat <<'__FAKE_EOF__'\n{stdout}\n__FAKE_EOF__\nexit 0"),
        );
    }

    /// Fail with `code`, printing `stderr` on fd 2.
    pub fn fail(&self, name: &str, code: i32, stderr: &str) {
        self.script(
            name,
            &format!("cat >&2 <<'__FAKE_EOF__'\n{stderr}\n__FAKE_EOF__\nexit {code}"),
        );
    }

    /// Succeed for every invocation except the one whose leading arguments
    /// are `action`, which fails.
    ///
    /// Matches on the subcommand, not "any argument contains", because
    /// `gh pr view --json title,body,comments,...` contains the word
    /// "comment" in its field list and a substring match fails the wrong
    /// call.
    pub fn ok_unless_action(&self, name: &str, action: &str, stdout: &str) {
        let words: Vec<&str> = action.split_whitespace().collect();
        let n = words.len();
        let joined = words.join(" ");
        self.script(
            name,
            &format!(
                "got=\"\"\nfor a in \"$@\"; do case \"$a\" in -*) break;; esac; got=\"${{got:+$got }}$a\"; n=$(echo \"$got\" | wc -w); [ \"$n\" -ge {n} ] && break; done\nif [ \"$got\" = \"{joined}\" ]; then echo \"fake {name}: refusing {joined}\" >&2; exit 1; fi\ncat <<'__FAKE_EOF__'\n{stdout}\n__FAKE_EOF__\nexit 0"
            ),
        );
    }

    /// Every recorded invocation, as argv vectors (`[name, arg, arg, ...]`).
    pub fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').map(str::to_owned).collect())
            .collect()
    }

    /// Invocations of `name` only.
    pub fn calls_to(&self, name: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.first().is_some_and(|n| n == name))
            .collect()
    }

    /// Whether any invocation of `name` contained `arg`.
    pub fn called_with(&self, name: &str, arg: &str) -> bool {
        self.calls_to(name)
            .iter()
            .any(|c| c.iter().any(|a| a == arg))
    }
}

impl Drop for FakeBins {
    fn drop(&mut self) {
        // SAFETY: one test per process.
        unsafe { std::env::set_var("PATH", &self.original_path) };
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("stat fake binary")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod fake binary");
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

// ---------------------------------------------------------------------------
// A backend that simulates a worker
// ---------------------------------------------------------------------------

type RunFn = Box<dyn Fn(&Path) -> RunResult + Send + Sync>;

/// A `Backend` whose `run()` is a closure over the worktree it is handed.
///
/// Use it to stage exactly what a worker would leave behind. `decide()`
/// always grants, so a test never has to satisfy the budget gate.
pub struct ScriptedBackend {
    run: RunFn,
}

impl ScriptedBackend {
    /// Run the given closure against the worktree.
    pub fn new(run: impl Fn(&Path) -> RunResult + Send + Sync + 'static) -> Self {
        Self { run: Box::new(run) }
    }

    /// A worker that writes `contents` to `name` in the worktree and exits 0.
    pub fn writing(name: &'static str, contents: &'static str) -> Self {
        Self::new(move |cwd| {
            std::fs::write(cwd.join(name), contents).expect("stage worker output");
            done()
        })
    }

    /// A worker that does nothing and exits 0.
    pub fn noop() -> Self {
        Self::new(|_| done())
    }
}

/// A successful, cheap `RunResult`.
pub fn done() -> RunResult {
    RunResult {
        exit_code: Some(0),
        killed_reason: None,
        tokens_new: 1_000,
        calls: 1,
        session_file: None,
        duration_s: 0.1,
        stdout_tail: String::new(),
        usage_delta: None,
    }
}

#[async_trait::async_trait]
impl Backend for ScriptedBackend {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let granted = Verdict::Granted {
            cap_tokens: None,
            reason: "test: always granted".to_owned(),
        };
        Ok(Outlook {
            normal: granted.clone(),
            prioritized: granted,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn run(
        &self,
        cwd: &Path,
        _prompt: &str,
        _cap_tokens: i64,
        _max_wall_s: i64,
        _job_class: JobClass,
    ) -> anyhow::Result<RunResult> {
        Ok((self.run)(cwd))
    }
}

// ---------------------------------------------------------------------------
// Git fixtures
// ---------------------------------------------------------------------------

/// A working repo with a bare `origin` it can fetch from, plus one feature
/// branch published on that origin.
///
/// The runners all start with `git fetch origin <head_ref>` followed by
/// `git worktree add`, so a bare origin is the minimum that lets them run
/// at all. Uses `-c` overrides rather than touching global git config.
pub struct GitRepo {
    pub origin: PathBuf,
    pub work: PathBuf,
    pub default_branch: String,
}

impl GitRepo {
    /// Build `origin` (bare) and `work` (a clone) under `dir`, with an
    /// initial commit on `main` and `branch` pushed to origin.
    pub fn with_branch(dir: &TempDir, branch: &str) -> Self {
        let origin = dir.join("origin.git");
        let work = dir.join("work");
        git(
            dir.path(),
            &[
                "init",
                "--bare",
                "-b",
                "main",
                origin.to_string_lossy().as_ref(),
            ],
        );
        git(
            dir.path(),
            &[
                "clone",
                origin.to_string_lossy().as_ref(),
                work.to_string_lossy().as_ref(),
            ],
        );
        std::fs::write(work.join("README.md"), "seed\n").expect("seed file");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "seed"]);
        git(&work, &["push", "origin", "main"]);
        git(&work, &["checkout", "-b", branch]);
        std::fs::write(work.join("CHANGE.md"), "change\n").expect("branch file");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "change"]);
        git(&work, &["push", "origin", branch]);
        git(&work, &["checkout", "main"]);
        Self {
            origin,
            work,
            default_branch: "main".to_owned(),
        }
    }
}

/// Run git in `cwd` with identity and hook config forced, so the fixture
/// does not depend on (or disturb) the developer's global git setup.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=test@hunter.invalid",
            "-c",
            "user.name=hunter test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}{}",
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Restores (or removes) an environment variable when dropped.
pub struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: one test per process.
        match &self.previous {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

// ---------------------------------------------------------------------------
// Domain fixtures
// ---------------------------------------------------------------------------

/// A `Repo` with every field populated, for tests that need one but do
/// not care what is in it. `Repo` has no `Default` in production and
/// should not grow one just for tests.
pub fn sample_repo() -> hunter::types::Repo {
    hunter::types::Repo {
        id: 1,
        name: "widget".to_owned(),
        url: "https://github.com/acme/widget".to_owned(),
        path: "/tmp/widget".to_owned(),
        forge: hunter::domain::ForgeName::Github,
        default_branch: "main".to_owned(),
        last_hunt_sha: Some("abc1234".to_owned()),
        last_hunt_at: Some(1),
        enabled: 1,
        added_at: 0,
        last_full_hunt_at: None,
        last_test_gap_at: None,
        last_dep_update_at: None,
        last_refactor_at: None,
        last_modernization_at: None,
        last_standards_at: None,
    }
}

/// A `Finding` of the given type with every optional field populated.
///
/// Populated rather than `None`-filled on purpose: a prompt builder that
/// reads a field only when it is `Some` would otherwise never exercise
/// that path, and the slot it fills would go unrendered.
pub fn sample_finding(kind: hunter::domain::FindingType) -> hunter::types::Finding {
    hunter::types::Finding {
        id: 42,
        kind,
        repo_id: 1,
        fingerprint: "fp-sample".to_owned(),
        file: Some("src/lib.rs".to_owned()),
        symbol: Some("do_thing".to_owned()),
        line: Some(17),
        severity: hunter::domain::Severity::Medium,
        confidence: 0.8,
        summary: "a summary".to_owned(),
        detail: Some("a detail".to_owned()),
        status: hunter::domain::FindingStatus::New,
        pr_url: Some("https://github.com/acme/widget/pull/7".to_owned()),
        created_at: 0,
        updated_at: 0,
        bug_class: Some(hunter::domain::BugClass::Logic),
        evidence_plan: Some("a plan".to_owned()),
        introduced_by: Some("deadbeef".to_owned()),
        rung_achieved: Some(2),
        verdict_reason: Some("a reason".to_owned()),
        budget_override: None,
        fix_attempts: 0,
        last_fix_failure: None,
        recheck_attempts: 0,
        last_recheck_failure: None,
        ecosystem: Some("npm".to_owned()),
        package: Some("left-pad".to_owned()),
        current_version: Some("1.2.0".to_owned()),
        latest_version: Some("1.3.0".to_owned()),
        update_type: Some("minor".to_owned()),
        security_advisory: Some("CVE-0000-0000".to_owned()),
        missing_tests: Some("[\"case a\"]".to_owned()),
        test_file: Some("tests/lib_test.rs".to_owned()),
        smell_type: Some("duplication".to_owned()),
        suggested_refactor: Some("extract a helper".to_owned()),
        modernization_class: Some("deprecated-api".to_owned()),
        current_approach: Some("the old way".to_owned()),
        proposed_approach: Some("the new way".to_owned()),
        standard_section: Some("Type safety / Domain types over primitives".to_owned()),
    }
}
