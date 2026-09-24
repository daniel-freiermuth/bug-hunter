//! Backend protocol — port of hunter/backend.py (BACKEND-CONTRACT.md §1).
//!
//! A backend answers: (1) may background work spend now, and up to how much
//! (`decide`); (2) run this job (`run`); (3) what's your status
//! (`keep_fresh`/`status_html`). All three are implemented by
//! `OmpScavengeBackend`.
//!
//! Decisions cross this boundary as data (`Outlook`); diagnostics cross as
//! presentation (an HTML fragment the backend fully owns). The typed
//! `WindowPanel` model behind that fragment stays private to the
//! `omp_scavenge` impl: the set of window dimensions a backend reports is
//! its own vocabulary, so typing it here would put one impl's shape into the
//! shared contract. The resulting coupling to the UI's CSS class names is
//! paid for by specifying the markup byte-exactly instead
//! (BACKEND-CONTRACT.md §2.3).

use async_trait::async_trait;

/// The two budget/model classes the scheduler collapses all job kinds into
/// at the backend boundary (`backend.JobClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobClass {
    Hunt,
    Fix,
}

impl JobClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hunt => "hunt",
            Self::Fix => "fix",
        }
    }
}

/// Granted/Denied verdict (`backend.Granted` / `backend.Denied`).
///
/// - `Granted.cap_tokens`: backend's own spend ceiling; None = no ceiling.
///   Core still min()s against its config caps — the backend never sees them.
/// - `Denied.retry_at`: epoch ms the denial is expected to resolve; None = no
///   informed estimate (callers use a generic 30-min backoff).
/// - reason strings are prose for notes/events/UI — never machine-matched
///   (tests substring-match only), but formats are kept byte-identical to
///   Python (`{:.2}` half-to-even rounding).
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Granted {
        cap_tokens: Option<i64>,
        reason: String,
    },
    Denied {
        reason: String,
        retry_at: Option<f64>,
    },
}

impl Verdict {
    pub fn is_granted(&self) -> bool {
        matches!(self, Self::Granted { .. })
    }
}

/// Paired verdicts (`backend.Outlook`).
///
/// INVARIANT: prioritized is at least as permissive as normal — if normal
/// is Granted, prioritized is Granted. The scheduler indexes
/// `outlook.prioritized if override else outlook.normal` and never tells
/// the backend which it wanted. `omp_scavenge` guarantees the invariant by
/// construction.
#[derive(Debug, Clone)]
pub struct Outlook {
    pub normal: Verdict,
    pub prioritized: Verdict,
}

/// What a backend may know about hunter's own job spend and observation
/// history (`backend.SpendLedger`). Implemented by Store on the sqlx pool
/// (Python's `ThreadLocalLedger` dissolves — the pool is already
/// thread-safe). Exact SQL: BACKEND-CONTRACT.md §1.6.
#[async_trait]
pub trait SpendLedger: Send + Sync {
    /// `SUM(COALESCE(estimated_tokens, cap_tokens, 0))` over running jobs.
    async fn running_estimate(&self) -> sqlx::Result<i64>;
    /// `SUM(tokens_new)` of non-running jobs finished strictly after `ts_ms`.
    async fn finished_since(&self, ts_ms: i64) -> sqlx::Result<i64>;
    /// Same, half-open (`start_ms`, `end_ms`].
    async fn finished_between(&self, start_ms: i64, end_ms: i64) -> sqlx::Result<i64>;
    /// INSERT INTO `window_log` (`observed_at` = now, `source_age_s` = `trunc(age_s)`).
    async fn log_window_observation(
        &self,
        limit_id: &str,
        used_fraction: Option<f64>,
        status: Option<&str>,
        resets_at: Option<i64>,
        age_s: f64,
    ) -> sqlx::Result<()>;
    /// Newest (`observed_at`, `used_fraction`) for `limit_id` within
    /// `resets_at` ± 5000 ms; None when absent OR `used_fraction` is NULL.
    async fn last_window_observation(
        &self,
        limit_id: &str,
        resets_at: i64,
    ) -> sqlx::Result<Option<(i64, f64)>>;
    /// INSERT INTO `calibration_samples` (`observed_at` = now).
    async fn record_calibration_sample(
        &self,
        limit_id: &str,
        window_resets_at: Option<i64>,
        used_fraction_delta: f64,
        hunter_tokens: i64,
    ) -> sqlx::Result<()>;
    /// Max tokens hunter ever spent inside one completed window cycle
    /// (NOT p75; `SpendLedger.estimate_capacity` / `Store.estimate_capacity`;
    /// the Python `_min_delta` param is dead and dropped here). None for unknown
    /// `limit_ids` or when no cycle had spend.
    async fn estimate_capacity(&self, limit_id: &str) -> sqlx::Result<Option<f64>>;
}

/// Subprocess seam for `keep_fresh`'s `omp usage` probes — injectable so the
/// 9 refresh tests control rc without spawning (mirrors Python tests
/// patching `hunter.util.run_cmd`). `run` mirrors `util::run_cmd`: merged
/// stdout+stderr, (124, msg) on timeout, (127, msg) on spawn error,
/// never fails.
pub trait Prober: Send + Sync {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String);
}

/// Real prober: delegates to `util::run_cmd`.
pub struct CmdProber;

impl Prober for CmdProber {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String) {
        crate::util::run_cmd(argv, timeout_s)
    }
}

/// The backend facade — harness, accounting and budget policy behind one
/// object, so the core never sees a provider's vocabulary.
///
/// Errors: Python lets sqlite/IO exceptions propagate to the caller's
/// catch-all (HTTP 500 / cycle error summary); Rust surfaces them as Err
/// with identical handling at the call sites.
#[async_trait]
pub trait Backend: Send + Sync {
    /// May background work spend now? `anticipated_tokens` = caller's
    /// pre-reservation for the job under decision.
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook>;
    /// Refresh stale accounting, log observations, record calibration.
    /// Returns true iff a probe was performed. Called by the daemon's
    /// prober tick.
    async fn keep_fresh(&self) -> anyhow::Result<bool>;
    /// Status HTML fragment for /`api/summary.backend_status_html`
    /// (innerHTML'd by the UI every 5 s; byte-parity spec in
    /// BACKEND-CONTRACT.md §2.3).
    async fn status_html(&self) -> anyhow::Result<String>;
    /// Execute a worker job. `cap_tokens` is the EFFECTIVE cap (already
    /// min'd by the caller); `job_class` drives model selection.
    async fn run(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        cap_tokens: i64,
        max_wall_s: i64,
        job_class: JobClass,
    ) -> anyhow::Result<crate::types::RunResult>;
}

/// Deterministic backend for router tests: no window data, ever.
pub struct NullBackend;

#[async_trait]
impl Backend for NullBackend {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let denied = Verdict::Denied {
            reason: "no window data -- deny until fresh".to_owned(),
            retry_at: None,
        };
        Ok(Outlook {
            normal: denied.clone(),
            prioritized: denied,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(r#"<div class="scv-note">No window data available</div>"#.to_owned())
    }

    async fn run(
        &self,
        _cwd: &std::path::Path,
        _prompt: &str,
        _cap_tokens: i64,
        _max_wall_s: i64,
        _job_class: JobClass,
    ) -> anyhow::Result<crate::types::RunResult> {
        anyhow::bail!("NullBackend cannot run workers")
    }
}
