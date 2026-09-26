//! Config loading — reads hunter/config.json (API-CONTRACT.md §12).
//! `root` is the hunter/ project dir (contains config.json, ui/, data/);
//! relative paths resolve against it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// On-disk shape of config.json — only the keys the serve path needs
/// (API-CONTRACT.md §12). Unknown keys are ignored (serde default).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    #[serde(rename = "workRoot")]
    work_root: Option<String>,
    #[serde(rename = "dbPath")]
    db_path: Option<String>,
    #[serde(rename = "ompBin")]
    omp_bin: Option<String>,
    #[serde(rename = "pollS")]
    poll_s: Option<f64>,
    #[serde(rename = "sessionGraceS")]
    session_grace_s: Option<u64>,
    serve: RawServe,
    budget: RawBudget,
    models: RawModels,
    backend: RawBackend,
    hunt: RawCaps,
    fix: RawCaps,
    scan: RawScan,
    modernization: RawScan,
    standards: RawScan,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawServe {
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBudget {
    #[serde(rename = "staleAfterS")]
    stale_after_s: Option<f64>,
    #[serde(rename = "cacheTtlS")]
    cache_ttl_s: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawModels {
    default: Option<String>,
    smol: Option<String>,
    hunt: Option<String>,
    fix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBackend {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawCaps {
    #[serde(rename = "maxWallS")]
    max_wall_s: Option<i64>,
    #[serde(rename = "maxFindings")]
    max_findings: Option<i64>,
    #[serde(rename = "rehuntDays")]
    rehunt_days: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawScan {
    #[serde(rename = "intervalDays")]
    interval_days: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The hunter/ project root this daemon reads from.
    pub root: PathBuf,
    pub work_root: PathBuf,
    pub db_path: PathBuf,
    pub serve_port: u16,
    /// root/ui — index.html + compiled app.js.
    pub ui_dir: PathBuf,
    // -- backend inputs (BACKEND-CONTRACT.md §3; `types.Config`) ----------
    pub omp_bin: String,
    /// budget.staleAfterS — `keep_fresh` gate + status stale tone. Prod
    /// default 300 (the Python test harness overrides to 1800).
    pub stale_after_s: f64,
    /// budget.cacheTtlS — `anticipated_tokens` warm/cold split.
    pub cache_ttl_s: f64,
    /// pollS — how often the harness re-reads a running worker's ledger.
    pub poll_s: f64,
    /// How long a worker may run before its session ledger must have
    /// appeared, after which the run is killed as unmetered. Injectable
    /// only so the accounting path is testable without a 2-minute test —
    /// `maxWallS` next to it in the same loop is a parameter for the same
    /// reason.
    pub session_grace_s: u64,
    pub model_default: Option<String>,
    pub model_smol: Option<String>,
    pub model_hunt: Option<String>,
    pub model_fix: Option<String>,
    /// backend.type discriminator; only "omp-scavenge" exists.
    pub backend_type: String,
    pub hunt_max_wall_s: i64,
    pub hunt_max_findings: i64,
    pub hunt_rehunt_days: i64,
    pub fix_max_wall_s: i64,
    /// scan.intervalDays — per-repo hunt cadence gate
    /// (`types.Config.scan_interval_days`, read in `Config.load`).
    pub scan_interval_days: f64,
    /// modernization.intervalDays (`types.Config.modernization_interval_days`, read in
    /// `Config.load`).
    pub modernization_interval_days: i64,
    /// standards.intervalDays — per-repo standards audit cadence gate.
    pub standards_interval_days: i64,
}

impl Config {
    /// (`model_hunt` if kind == "hunt" else `model_fix`) or `model_default`
    /// (`types.Config.model_for`).
    pub fn model_for(&self, kind: &str) -> Option<&str> {
        let specific = if kind == "hunt" {
            self.model_hunt.as_deref()
        } else {
            self.model_fix.as_deref()
        };
        specific.or(self.model_default.as_deref())
    }
}

impl Config {
    /// Load <root>/config.json (missing file = all defaults, like Python).
    /// Keys: workRoot (default "data"), dbPath (default "data/hunter.db"),
    /// serve.port (default 8377). Relative paths resolve against `root`.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear validation pass per config key; each `ensure!` \
                  carries the prose explaining its bound, and splitting it \
                  into per-section helpers would scatter that reasoning \
                  across functions that are each called exactly once"
    )]
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        const MS_PER_DAY: i64 = 86_400_000;
        let cfg_path = root.join("config.json");
        let raw = match std::fs::read_to_string(&cfg_path) {
            Ok(text) => serde_json::from_str::<RawConfig>(&text)
                .with_context(|| format!("parsing {}", cfg_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => RawConfig::default(),
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context(format!("reading {}", cfg_path.display()))
                );
            }
        };
        let resolve = |p: &str| -> PathBuf {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_owned()
            } else {
                root.join(p)
            }
        };
        // pollS drives the harness meter tick (harness.rs); zero busy-loops
        // the poll and negative/NaN panics Duration::from_secs_f64.
        let poll_s = raw.poll_s.unwrap_or(2.0);
        anyhow::ensure!(
            poll_s.is_finite() && poll_s > 0.0,
            "{}: pollS must be a finite number greater than 0 (got {poll_s})",
            cfg_path.display()
        );
        let session_grace_s = raw.session_grace_s.unwrap_or(120);
        anyhow::ensure!(
            session_grace_s > 0,
            "{}: sessionGraceS must be greater than 0",
            cfg_path.display()
        );
        // A zero or negative wall-clock limit trips `elapsed >= max_wall_s`
        // on the worker's first meter tick, so the run is killed before it
        // does anything. Zero is not a way to disable the limit, it is the
        // tightest possible one.
        let limit = |field: &str, value: i64| -> anyhow::Result<i64> {
            anyhow::ensure!(
                value > 0,
                "{}: {field} must be greater than 0 (got {value})",
                cfg_path.display()
            );
            Ok(value)
        };
        let hunt_max_wall_s = limit("hunt.maxWallS", raw.hunt.max_wall_s.unwrap_or(1800))?;
        let fix_max_wall_s = limit("fix.maxWallS", raw.fix.max_wall_s.unwrap_or(2700))?;
        // staleAfterS is compared as `age_s <= stale_after_s`, so 0 has a
        // defined meaning (only a just-recorded window counts as fresh).
        // A negative makes every window permanently stale, which makes
        // `keep_fresh` probe on every call.
        let stale_after_s = raw.budget.stale_after_s.unwrap_or(300.0);
        anyhow::ensure!(
            stale_after_s.is_finite() && stale_after_s >= 0.0,
            "{}: budget.staleAfterS must be a finite number greater than or equal to 0 (got {stale_after_s})",
            cfg_path.display()
        );
        // Cadences are validated at the precision the scheduler actually
        // uses: it converts days to milliseconds and compares against
        // elapsed time, so a value that rounds to a non-positive interval
        // makes its job eligible on every single cycle rather than "never"
        // or "rarely", which is the opposite of what an operator typing a
        // small number intends.
        #[allow(clippy::cast_precision_loss)]
        let max_days = (i64::MAX / MS_PER_DAY) as f64;
        let scan_interval_days = raw.scan.interval_days.unwrap_or(1.0);
        anyhow::ensure!(
            scan_interval_days.is_finite()
                && scan_interval_days > 0.0
                && scan_interval_days <= max_days
                && (scan_interval_days * MS_PER_DAY as f64) >= 1.0,
            "{}: scan.intervalDays must be a finite number greater than 0 and at most {max_days} \
             (got {scan_interval_days})",
            cfg_path.display()
        );
        // These two are whole days by the time the scheduler sees them, so
        // a fraction is truncated — 0.5 silently becomes 0, i.e. every
        // cycle. Reject it rather than reinterpret it.
        let whole_days = |field: &str, raw_days: Option<f64>| -> anyhow::Result<i64> {
            let d = raw_days.unwrap_or(30.0);
            anyhow::ensure!(
                d.is_finite() && d >= 1.0 && d <= max_days && d.fract() == 0.0,
                "{}: {field} must be a whole number of days, at least 1 and at most {max_days} \
                 (got {d})",
                cfg_path.display()
            );
            #[allow(clippy::cast_possible_truncation)]
            Ok(d as i64)
        };
        let modernization_interval_days = whole_days(
            "modernization.intervalDays",
            raw.modernization.interval_days,
        )?;
        let standards_interval_days =
            whole_days("standards.intervalDays", raw.standards.interval_days)?;

        Ok(Self {
            root: root.to_owned(),
            work_root: resolve(raw.work_root.as_deref().unwrap_or("data")),
            db_path: resolve(raw.db_path.as_deref().unwrap_or("data/hunter.db")),
            serve_port: raw.serve.port.unwrap_or(8377),
            ui_dir: root.join("ui"),
            omp_bin: raw.omp_bin.unwrap_or_else(|| "omp".to_owned()),
            stale_after_s,
            cache_ttl_s: raw.budget.cache_ttl_s.unwrap_or(3600.0),
            poll_s,
            session_grace_s,
            model_default: raw.models.default,
            model_smol: raw.models.smol,
            model_hunt: raw.models.hunt,
            model_fix: raw.models.fix,
            backend_type: raw
                .backend
                .kind
                .unwrap_or_else(|| "omp-scavenge".to_owned()),
            hunt_max_wall_s,
            hunt_max_findings: raw.hunt.max_findings.unwrap_or(8),
            hunt_rehunt_days: raw.hunt.rehunt_days.unwrap_or(90),
            fix_max_wall_s,
            scan_interval_days,
            modernization_interval_days,
            standards_interval_days,
        })
    }
}
