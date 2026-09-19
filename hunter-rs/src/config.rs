//! Config loading — reads the SAME hunter/config.json the Python side owns
//! (API-CONTRACT.md §12). `root` is the Python project dir (contains
//! config.json, ui/, data/); relative paths resolve against it.

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
    #[serde(rename = "capNewTokens")]
    cap_new_tokens: Option<i64>,
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
    /// The hunter/ project root (Python side) this serve reads from.
    pub root: PathBuf,
    pub work_root: PathBuf,
    pub db_path: PathBuf,
    pub serve_port: u16,
    /// root/ui — index.html + compiled app.js.
    pub ui_dir: PathBuf,
    // -- backend inputs (BACKEND-CONTRACT.md §3; types.py:172-243) ----------
    pub omp_bin: String,
    /// budget.staleAfterS — `keep_fresh` gate + status stale tone. Prod
    /// default 300 (the Python test harness overrides to 1800).
    pub stale_after_s: f64,
    /// budget.cacheTtlS — `anticipated_tokens` warm/cold split.
    pub cache_ttl_s: f64,
    /// pollS — harness meter tick (round 3).
    pub poll_s: f64,
    pub model_default: Option<String>,
    pub model_smol: Option<String>,
    pub model_hunt: Option<String>,
    pub model_fix: Option<String>,
    /// backend.type discriminator; only "omp-scavenge" exists.
    pub backend_type: String,
    pub hunt_cap_tokens: i64,
    pub hunt_max_wall_s: i64,
    pub hunt_max_findings: i64,
    pub hunt_rehunt_days: i64,
    pub fix_cap_tokens: i64,
    pub fix_max_wall_s: i64,
    /// scan.intervalDays — per-repo hunt cadence gate (types.py:181/217).
    pub scan_interval_days: f64,
    /// modernization.intervalDays (types.py:182/218).
    pub modernization_interval_days: i64,
    /// standards.intervalDays — per-repo standards audit cadence gate.
    pub standards_interval_days: i64,
}

impl Config {
    /// (`model_hunt` if kind == "hunt" else `model_fix`) or `model_default`
    /// (types.py:195-197).
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
    pub fn load(root: &Path) -> anyhow::Result<Self> {
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
        Ok(Self {
            root: root.to_owned(),
            work_root: resolve(raw.work_root.as_deref().unwrap_or("data")),
            db_path: resolve(raw.db_path.as_deref().unwrap_or("data/hunter.db")),
            serve_port: raw.serve.port.unwrap_or(8377),
            ui_dir: root.join("ui"),
            omp_bin: raw.omp_bin.unwrap_or_else(|| "omp".to_owned()),
            stale_after_s: raw.budget.stale_after_s.unwrap_or(300.0),
            cache_ttl_s: raw.budget.cache_ttl_s.unwrap_or(3600.0),
            poll_s,
            model_default: raw.models.default,
            model_smol: raw.models.smol,
            model_hunt: raw.models.hunt,
            model_fix: raw.models.fix,
            backend_type: raw
                .backend
                .kind
                .unwrap_or_else(|| "omp-scavenge".to_owned()),
            hunt_cap_tokens: raw.hunt.cap_new_tokens.unwrap_or(200_000),
            hunt_max_wall_s: raw.hunt.max_wall_s.unwrap_or(1800),
            hunt_max_findings: raw.hunt.max_findings.unwrap_or(8),
            hunt_rehunt_days: raw.hunt.rehunt_days.unwrap_or(90),
            fix_cap_tokens: raw.fix.cap_new_tokens.unwrap_or(150_000),
            fix_max_wall_s: raw.fix.max_wall_s.unwrap_or(2700),
            scan_interval_days: raw.scan.interval_days.unwrap_or(1.0),
            modernization_interval_days: raw.modernization.interval_days.map_or(30, |d| d as i64),
            standards_interval_days: raw.standards.interval_days.map_or(30, |d| d as i64),
        })
    }
}
