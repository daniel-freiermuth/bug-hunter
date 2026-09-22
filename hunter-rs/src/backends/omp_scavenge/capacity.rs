//! Window reading + pure ramp math (capacity.py, BACKEND-CONTRACT.md §2.1).
//! Pure fns stay parameterized on `now_ms` (deterministic tests);
//! `read_windows` takes the agent.db path explicitly (tests point it at a
//! fixture).

use super::provider::LlmProvider;
use sqlx::Row;
use std::collections::BTreeMap;

use std::path::Path;

/// THE tunable: human headroom at 5h-window start (`capacity.HEADROOM_MS`).
pub const HEADROOM_MS: i64 = 30 * 60 * 1000;
pub const WEEK_MS: i64 = 604_800_000;
pub const FIVE_H_MS: i64 = 18_000_000;
/// 5h ramp span after headroom: 4.5 h.
pub const RAMP_MS: i64 = FIVE_H_MS - HEADROOM_MS;

/// One provider window as read from omp's usage mirror.
#[derive(Debug, Clone)]
pub struct WindowState {
    pub limit_id: String,
    pub used_fraction: Option<f64>,
    pub status: Option<String>,
    pub resets_at: Option<i64>,
    /// When omp probed (epoch ms) — or, for rolled-forward expired
    /// cycles, the current cycle's start boundary.
    pub recorded_at: i64,
    pub age_s: f64,
}

/// Default agent.db location (`capacity.OMP_AGENT_DB`); callers may override.
pub fn default_agent_db() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    home.join(".omp/agent/agent.db")
}

/// Read newest `usage_history` row per anthropic:% `limit_id`; roll expired
/// account-wide cycles forward (used=0, status=ok, `recorded_at=cycle`
/// start), DROP expired per-model-class rows. Missing file or ANY sqlite
/// error -> empty map. `BTreeMap`: deny-reason precedence needs ascending
/// `limit_id` order.
///
/// An empty map is indistinguishable from "no window data" downstream:
/// `decide` denies until fresh and `keep_fresh` probes forever. A genuine
/// read failure therefore looks exactly like an idle daemon unless it is
/// logged here. A missing file or a row-less table is not a failure and
/// stays silent.
pub fn read_windows(agent_db: &Path, now_ms: i64) -> BTreeMap<String, WindowState> {
    read_windows_for(agent_db, LlmProvider::Anthropic, now_ms)
}

pub fn read_windows_for(
    agent_db: &Path,
    provider: LlmProvider,
    now_ms: i64,
) -> BTreeMap<String, WindowState> {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::error!(
            agent_db = %agent_db.display(),
            "read_windows called outside a Tokio runtime — usage windows unavailable"
        );
        return BTreeMap::new();
    };
    match handle.block_on(read_windows_async(agent_db, provider, now_ms)) {
        Ok(windows) => windows,
        Err(err) => {
            tracing::error!(
                agent_db = %agent_db.display(),
                error = %err,
                "reading omp usage windows failed — usage windows unavailable"
            );
            BTreeMap::new()
        }
    }
}

/// Async inner: opens `agent_db` read-only, queries newest row per
/// `limit_id` via window function, rolls forward expired account-wide
/// cycles, drops expired per-model-class rows.
async fn read_windows_async(
    agent_db: &Path,
    provider: LlmProvider,
    now_ms: i64,
) -> Result<BTreeMap<String, WindowState>, Box<dyn std::error::Error + Send + Sync>> {
    use sqlx::sqlite::SqliteConnectOptions;
    if !agent_db.exists() {
        return Ok(BTreeMap::new());
    }

    let opts = SqliteConnectOptions::new()
        .filename(agent_db)
        .read_only(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await?;

    let limits = provider.windows();
    let rows = sqlx::query(
        "SELECT limit_id, used_fraction, status, resets_at, recorded_at \
         FROM ( \
           SELECT limit_id, used_fraction, status, resets_at, recorded_at, \
                  ROW_NUMBER() OVER (PARTITION BY limit_id ORDER BY recorded_at DESC) AS rn \
           FROM usage_history \
           WHERE provider = ? AND limit_id IN (?, ?) \
         ) WHERE rn = 1",
    )
    .bind(provider.name())
    .bind(limits.short.limit_id)
    .bind(limits.long.limit_id)
    .fetch_all(&pool)
    .await?;

    pool.close().await;

    let mut result = BTreeMap::new();

    for row in rows {
        // agent.db belongs to omp, so its schema and values are foreign
        // input: `Row::get` would panic on a NULL or a wrongly-typed cell
        // and unwind the caller's `spawn_blocking`, turning one bad cell
        // into a JoinError on every `/api/summary` poll and every worker
        // pre-snapshot. A decode failure abandons the WHOLE snapshot
        // rather than the offending row, because a partial map is unsafe
        // in a way an empty one is not: `decide_inner` only refuses when
        // the map is empty, so dropping just an undecodable anthropic:5h
        // row would let spending be granted against a window nobody can
        // see, while the empty map is the designed "capacity unknown"
        // state (deny until fresh, probe, "no window data" in the UI).
        let limit_id: String = row.try_get("limit_id")?;
        let used_fraction: Option<f64> = row.try_get("used_fraction")?;
        let status: Option<String> = row.try_get("status")?;
        let resets_at: Option<i64> = row.try_get("resets_at")?;
        let recorded_at: i64 = row.try_get("recorded_at")?;

        match resets_at {
            // Truthy resets_at (> 0) that has expired (<= now_ms).
            // Some(0) is falsy like Python's `not resets_at`.
            Some(r) if r > 0 && r <= now_ms => {
                let period = if limit_id == limits.short.limit_id {
                    limits.short.period_ms
                } else if limit_id == limits.long.limit_id {
                    limits.long.period_ms
                } else {
                    continue;
                };
                // Roll forward: advance resets_at by period until > now,
                // recording each step as the current cycle's start.
                let mut new_resets = r;
                let mut new_recorded = recorded_at;
                while new_resets <= now_ms {
                    new_recorded = new_resets;
                    new_resets += period;
                }
                result.insert(
                    limit_id.clone(),
                    WindowState {
                        limit_id,
                        used_fraction: Some(0.0),
                        status: Some("ok".to_owned()),
                        resets_at: Some(new_resets),
                        recorded_at: new_recorded,
                        age_s: (now_ms - new_recorded) as f64 / 1000.0,
                    },
                );
            }
            _ => {
                // Not expired, NULL resets_at, or Some(0) → keep verbatim.
                result.insert(
                    limit_id.clone(),
                    WindowState {
                        limit_id,
                        used_fraction,
                        status,
                        resets_at,
                        recorded_at,
                        age_s: (now_ms - recorded_at) as f64 / 1000.0,
                    },
                );
            }
        }
    }

    Ok(result)
}

/// not `resets_at` or expired -> 1.0; else min(elapsed/week, 1.0).
pub fn ramp_7d(resets_at: Option<i64>, now_ms: i64) -> f64 {
    match resets_at {
        None | Some(0) => 1.0,
        // An expired window needs no arm of its own: elapsed is then at
        // least a full week, so the `.min(1.0)` below already returns 1.0.
        // A guard here would be unfalsifiable — no input distinguishes it.
        Some(r) => {
            let elapsed = (now_ms - (r - WEEK_MS)) as f64;
            (elapsed / WEEK_MS as f64).min(1.0)
        }
    }
}

/// not `resets_at` or expired -> None (no active window: opener allowed);
/// else max(0, (elapsed - HEADROOM) / RAMP).
pub fn ramp_5h(resets_at: Option<i64>, now_ms: i64) -> Option<f64> {
    match resets_at {
        None | Some(0) => None,
        Some(r) if r <= now_ms => None, // expired
        Some(r) => {
            let elapsed = (FIVE_H_MS - (r - now_ms)) as f64;
            Some(((elapsed - HEADROOM_MS as f64) / RAMP_MS as f64).max(0.0))
        }
    }
}

/// Exact inverse of `ramp_7d`; None when `resets_at` is None/0.
pub fn retry_at_7d(resets_at: Option<i64>, effective_used: f64) -> Option<f64> {
    match resets_at {
        None | Some(0) => None,
        Some(r) => {
            let raw = (r - WEEK_MS) as f64 + effective_used * WEEK_MS as f64;
            Some(raw.min(r as f64))
        }
    }
}

pub fn retry_at_5h(resets_at: Option<i64>, effective_used: f64) -> Option<f64> {
    match resets_at {
        None | Some(0) => None,
        Some(r) => {
            let raw = (r - FIVE_H_MS) as f64 + HEADROOM_MS as f64 + effective_used * RAMP_MS as f64;
            Some(raw.min(r as f64))
        }
    }
}

/// status == "exhausted" clamps to exactly 1.0 (hard-stop signal; raw
/// value unreliable); else `used_fraction` (call sites guarantee Some) +
/// inflight reservation.
pub fn effective_used(w: &WindowState, inflight_reservation: f64) -> f64 {
    let used = if w.status.as_deref() == Some("exhausted") {
        1.0
    } else {
        w.used_fraction.unwrap_or(0.0)
    };
    used + inflight_reservation
}
