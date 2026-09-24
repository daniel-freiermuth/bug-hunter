//! `OmpScavengeBackend` facade (facade.py, BACKEND-CONTRACT.md §2.2-2.3).
//! Bundles the harness, the window accounting and the scavenging policy
//! behind the single `Backend` the scheduler sees.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::capacity::{
    self, FIVE_H_MS, HEADROOM_MS, WEEK_MS, WindowState, effective_used, ramp_5h, ramp_7d,
    retry_at_5h, retry_at_7d,
};
use crate::backend::SpendLedger;
use crate::backend::{Backend, JobClass, Outlook, Prober, Verdict};
use crate::config::Config;

/// 200 k ≈ 10% of a 5h window (`facade._TOK_PER_FRAC_5H`).
const TOK_PER_FRAC_5H: f64 = 2_000_000.0;
/// 5h / 7d period ratio ≈ 0.0297619 (`facade._5H_7D_RATIO`).
const RATIO_5H_7D: f64 = FIVE_H_MS as f64 / WEEK_MS as f64;

/// Calibration horizons: iteration order "5h" then "7d" (`facade._CALIBRATION_DURATIONS_MS`).
const CALIBRATION_HORIZONS: &[(&str, i64)] = &[("5h", FIVE_H_MS), ("7d", WEEK_MS)];

/// All Anthropic-specific vocabulary (5h/7d windows, ramp math, probe
/// staleness) is confined to this type.
pub struct OmpScavengeBackend {
    pub cfg: Config,
    pub ledger: Arc<dyn SpendLedger>,
    /// omp's usage mirror; `default_agent_db()` in production, a fixture in
    /// tests.
    pub agent_db: PathBuf,
    /// `keep_fresh`'s `omp usage` subprocess seam.
    pub prober: Arc<dyn Prober>,
}

// ---- private helpers (facade.py functions) --------------------------------

/// html.escape equivalent: & < > " ' (`facade._esc`).
///
/// Free functions rather than associated ones: they are pure, and
/// `render_status` needs them without a backend instance.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Format token count (`facade.OmpScavengeBackend._fmt_tokens`).
fn fmt_tokens(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.0}k", n / 1_000.0)
    } else {
        format!("{}", n as i64)
    }
}

/// Token count as a fraction of a window's capacity. A capacity that is
/// not positive carries no meaningful fraction.
fn as_fraction(tokens: i64, capacity: f64) -> f64 {
    if capacity > 0.0 {
        tokens as f64 / capacity
    } else {
        0.0
    }
}

/// The inflight reservation for one `decide` call, as a fraction of each
/// window's capacity. Field names match the Python dataclass.
///
/// Two reservations, because the gate and the cap ask different questions
/// of the same number. `gate_*` counts `anticipated` in: the gate asks "if
/// this job also runs, does the window cross its ramp?", and the job's own
/// cost belongs in that answer. `budget_*` leaves it out, because the
/// headroom a grant computes IS the job's budget — charging `anticipated`
/// against the reservation and then handing the job what remains counts
/// that cost twice, giving it `headroom - anticipated` tokens to do work
/// worth `anticipated`. A job that clears the gate by a hair would then be
/// capped far below its own session floor and killed by the watchdog
/// having produced nothing, while the tokens it did spend still count
/// against the window. Taking every cap from `budget_*` makes
/// `cap >= anticipated` hold whenever the gate grants.
#[derive(Clone, Copy)]
struct Reservation {
    /// Gate view: running + finished-since-probe + `anticipated`.
    gate_5h: f64,
    gate_7d: f64,
    /// Cap view: the same, without `anticipated`.
    budget_5h: f64,
    budget_7d: f64,
    /// Capacities the fractions were taken against.
    cap_5h: f64,
    cap_7d: f64,
}

impl OmpScavengeBackend {
    /// Fraction → token cap (`facade.OmpScavengeBackend._frac_to_tokens`).
    ///
    /// Both caps come from `unaccounted_fraction`, which derives them the
    /// way Python does: each window's own `estimate_capacity`, falling
    /// back to the 5h constant and — for 7d — to 5h ÷ period ratio when
    /// there is not yet history to estimate from. Python recomputes them
    /// here; passing them in is the same value with fewer ledger reads.
    #[allow(clippy::similar_names)]
    fn frac_to_tokens(frac: f64, dim: &str, cap_5h: f64, cap_7d: f64) -> i64 {
        if dim == "5h" || dim.contains(":5h") {
            (frac * cap_5h) as i64
        } else {
            (frac * cap_7d) as i64
        }
    }

    /// Inflight reservation as fraction per window
    /// (`facade.OmpScavengeBackend._unaccounted_fraction` / `_reservation`).
    /// See [`Reservation`] for why there are two of them.
    #[allow(clippy::similar_names)]
    async fn unaccounted_fraction(
        &self,
        windows: &BTreeMap<String, WindowState>,
        anticipated: i64,
    ) -> anyhow::Result<Reservation> {
        let running = self.ledger.running_estimate().await?;

        let fallback = windows.values().map(|w| w.recorded_at).min().unwrap_or(0);
        let probe_at_5h = windows
            .get("anthropic:5h")
            .map_or(fallback, |w| w.recorded_at);
        let probe_at_7d = windows
            .get("anthropic:7d")
            .map_or(fallback, |w| w.recorded_at);

        let base = running + anticipated;
        let unaccounted_5h = base + self.ledger.finished_since(probe_at_5h).await?;
        let unaccounted_7d = base + self.ledger.finished_since(probe_at_7d).await?;

        let cap_5h = self
            .ledger
            .estimate_capacity("anthropic:5h")
            .await?
            .filter(|&v| v > 0.0)
            .unwrap_or(TOK_PER_FRAC_5H);
        let cap_7d = self
            .ledger
            .estimate_capacity("anthropic:7d")
            .await?
            .filter(|&v| v > 0.0)
            .unwrap_or(cap_5h / RATIO_5H_7D);

        let gate_5h = as_fraction(unaccounted_5h, cap_5h);
        let gate_7d = as_fraction(unaccounted_7d, cap_7d);
        let budget_5h = as_fraction(unaccounted_5h - anticipated, cap_5h);
        let budget_7d = as_fraction(unaccounted_7d - anticipated, cap_7d);
        Ok(Reservation {
            gate_5h,
            gate_7d,
            budget_5h,
            budget_7d,
            cap_5h,
            cap_7d,
        })
    }

    /// Single verdict: 7d ramps first, then 5h (`facade.OmpScavengeBackend._decide_inner`).
    fn decide_inner(
        windows: &BTreeMap<String, WindowState>,
        resv: Reservation,
        prio: bool,
        now_ms: i64,
    ) -> Verdict {
        if windows.is_empty() {
            return Verdict::Denied {
                reason: "no window data -- deny until fresh".to_owned(),
                retry_at: None,
            };
        }

        // 7d pass — BTreeMap order = ascending limit_id; first over-ramp
        // :7d lid wins the reason string.
        for (lid, w) in windows {
            if !lid.contains(":7d") {
                continue;
            }
            // Skip if not exhausted and used_fraction is None.
            if w.status.as_deref() != Some("exhausted") && w.used_fraction.is_none() {
                continue;
            }
            // Belt-and-braces: skip expired cycles.
            if let Some(r) = w.resets_at
                && r > 0
                && r <= now_ms
            {
                continue;
            }

            let elapsed_frac = ramp_7d(w.resets_at, now_ms);
            let eff = effective_used(w, resv.gate_7d);

            if eff >= elapsed_frac {
                let is_exhausted = w.status.as_deref() == Some("exhausted");
                let retry = if is_exhausted {
                    w.resets_at.map(|r| r as f64)
                } else {
                    retry_at_7d(w.resets_at, eff)
                };
                let u = eff - resv.gate_7d;
                let reason = format!(
                    "{lid}: used {u:.2} + unaccounted {res:.2} = {eff:.2} >= ramp {elapsed_frac:.2}",
                    res = resv.gate_7d
                );

                if prio && !is_exhausted {
                    // Headroom for THIS job, so the reservation it is
                    // measured against must not already contain it.
                    let headroom_frac = (1.0 - effective_used(w, resv.budget_7d)).max(0.0);
                    let cap = Self::frac_to_tokens(headroom_frac, "7d", resv.cap_5h, resv.cap_7d);
                    if cap <= 0 {
                        return Verdict::Denied {
                            reason,
                            retry_at: retry,
                        };
                    }
                    return Verdict::Granted {
                        cap_tokens: Some(cap),
                        reason: format!("prio override ({reason})"),
                    };
                }
                return Verdict::Denied {
                    reason,
                    retry_at: retry,
                };
            }
        }

        // 5h pass.
        if let Some(w5) = windows.get("anthropic:5h")
            && (w5.status.as_deref() == Some("exhausted") || w5.used_fraction.is_some())
            && let Some(allowed) = ramp_5h(w5.resets_at, now_ms)
        {
            let eff = effective_used(w5, resv.gate_5h);
            if eff >= allowed {
                let is_exhausted = w5.status.as_deref() == Some("exhausted");
                let retry = if is_exhausted {
                    w5.resets_at.map(|r| r as f64)
                } else {
                    retry_at_5h(w5.resets_at, eff)
                };
                let u = eff - resv.gate_5h;
                let reason = format!(
                    "5h: used {u:.2} + unaccounted {res:.2} = {eff:.2} >= ramp {allowed:.2}",
                    res = resv.gate_5h
                );

                if prio && !is_exhausted {
                    // Headroom for THIS job, so the reservation it is
                    // measured against must not already contain it.
                    let headroom_frac = (1.0 - effective_used(w5, resv.budget_5h)).max(0.0);
                    let cap = Self::frac_to_tokens(headroom_frac, "5h", resv.cap_5h, resv.cap_7d);
                    if cap <= 0 {
                        return Verdict::Denied {
                            reason,
                            retry_at: retry,
                        };
                    }
                    return Verdict::Granted {
                        cap_tokens: Some(cap),
                        reason: format!("prio override ({reason})"),
                    };
                }
                return Verdict::Denied {
                    reason,
                    retry_at: retry,
                };
            }
            // allowed is None → opener / no active window → skip.
        }

        // All passed — compute headroom (fresh now_ms: second clock read).
        let headroom_now = crate::util::now_ms();
        let headroom = Self::compute_headroom(windows, resv, prio, headroom_now);
        Verdict::Granted {
            cap_tokens: headroom,
            reason: "ok".to_owned(),
        }
    }

    /// Min headroom in tokens across windows (`facade.OmpScavengeBackend._compute_headroom`).
    ///
    /// Measured against `budget_*` (see [`Reservation`]): this headroom is
    /// what the anticipated job is allowed to spend, not what is left once
    /// it has spent.
    fn compute_headroom(
        windows: &BTreeMap<String, WindowState>,
        resv: Reservation,
        prio: bool,
        now_ms: i64,
    ) -> Option<i64> {
        let mut caps: Vec<i64> = Vec::new();

        for (lid, w) in windows {
            if w.used_fraction.is_none() {
                continue;
            }
            if lid.contains(":7d") {
                let ceiling = if prio {
                    1.0
                } else {
                    ramp_7d(w.resets_at, now_ms)
                };
                let frac = (ceiling - effective_used(w, resv.budget_7d)).max(0.0);
                let tok = Self::frac_to_tokens(frac, "7d", resv.cap_5h, resv.cap_7d);
                caps.push(tok);
            } else if lid.contains(":5h")
                && let Some(allowed) = ramp_5h(w.resets_at, now_ms)
            {
                let ceiling = if prio { 1.0 } else { allowed };
                let frac = (ceiling - effective_used(w, resv.budget_5h)).max(0.0);
                let tok = Self::frac_to_tokens(frac, "5h", resv.cap_5h, resv.cap_7d);
                caps.push(tok);
            }
        }

        caps.iter().copied().min()
    }

    /// Log observations + record calibration (`facade.OmpScavengeBackend._observe`).
    async fn observe(&self, windows: &BTreeMap<String, WindowState>) -> anyhow::Result<()> {
        let now = crate::util::now_ms();

        for w in windows.values() {
            // Determine calibration horizon: "5h" then "7d" in iteration
            // order; no lid contains both substrings.
            let horizon = CALIBRATION_HORIZONS
                .iter()
                .find(|(h, _)| w.limit_id.contains(&format!(":{h}")))
                .copied();

            // Calibration: requires horizon + resets_at + used_fraction all
            // present, a previous observation in the same cycle, fraction
            // increased, and within the period.
            if let Some((_h_name, h_dur)) = horizon
                && let (Some(resets), Some(used_frac)) = (w.resets_at, w.used_fraction)
                && let Some((prev_obs, prev_frac)) = self
                    .ledger
                    .last_window_observation(&w.limit_id, resets)
                    .await?
                && used_frac > prev_frac
                && (now - prev_obs) <= h_dur
            {
                let tok = self.ledger.finished_between(prev_obs, now).await?;
                if tok > 0 {
                    self.ledger
                        .record_calibration_sample(
                            &w.limit_id,
                            Some(resets),
                            used_frac - prev_frac,
                            tok,
                        )
                        .await?;
                }
            }

            // ALWAYS log observation.
            self.ledger
                .log_window_observation(
                    &w.limit_id,
                    w.used_fraction,
                    w.status.as_deref(),
                    w.resets_at,
                    w.age_s,
                )
                .await?;
        }

        Ok(())
    }

    /// Max `used_fraction` across 7d windows, or None (`facade.OmpScavengeBackend._usage_snapshot`).
    /// Blocking — meant for `spawn_blocking`.
    fn _usage_snapshot_sync(agent_db: &std::path::Path) -> Option<f64> {
        let now = crate::util::now_ms();
        let windows = capacity::read_windows(agent_db, now);
        windows
            .iter()
            .filter(|(k, _)| k.contains(":7d"))
            .filter_map(|(_, w)| w.used_fraction)
            .reduce(f64::max)
    }
}

// ---- public testing seam --------------------------------------------------

impl OmpScavengeBackend {
    /// Testing seam: runs decide logic on pre-built windows (skips
    /// `read_windows`).  Not part of the Backend trait.
    pub async fn decide_with_windows(
        &self,
        windows: &BTreeMap<String, WindowState>,
        anticipated_tokens: i64,
    ) -> anyhow::Result<Outlook> {
        let now = crate::util::now_ms();
        let resv = self
            .unaccounted_fraction(windows, anticipated_tokens)
            .await?;
        let normal = Self::decide_inner(windows, resv, false, now);

        let prioritized = match &normal {
            Verdict::Granted {
                cap_tokens: normal_cap,
                ..
            } => {
                // Normal granted → prioritized = normal, with possible
                // prio-headroom upgrade (fresh now_ms).
                let prio_now = crate::util::now_ms();
                let prio_headroom = Self::compute_headroom(windows, resv, true, prio_now);
                match (prio_headroom, normal_cap) {
                    (Some(ph), None) => Verdict::Granted {
                        cap_tokens: Some(ph),
                        reason: "ok".to_owned(),
                    },
                    (Some(ph), Some(nc)) if ph > *nc => Verdict::Granted {
                        cap_tokens: Some(ph),
                        reason: "ok".to_owned(),
                    },
                    _ => normal.clone(),
                }
            }
            Verdict::Denied { .. } => {
                // Normal denied → compute prioritized with prio=true.
                Self::decide_inner(windows, resv, true, now)
            }
        };

        Ok(Outlook {
            normal,
            prioritized,
        })
    }

    /// Staleness predicate behind `keep_fresh`'s probe gate: fresh iff the
    /// anthropic:5h window exists and its age is within `stale_after_s`,
    /// inclusive — an age landing exactly on the threshold is still fresh.
    /// A missing 5h window (or no windows at all) is not fresh.
    ///
    /// Split out from `keep_fresh` so the boundary is observable without
    /// the wall-clock read that derives `age_s`.
    pub fn is_fresh(&self, windows: &BTreeMap<String, WindowState>) -> bool {
        windows
            .get("anthropic:5h")
            .is_some_and(|w5| w5.age_s <= self.cfg.stale_after_s)
    }
}

/// Returned verbatim when there is no window data at all.
pub const NO_WINDOW_DATA: &str = r#"<div class="scv-note">No window data available</div>"#;

/// Everything [`render_status`] needs, resolved by `status_html` from the
/// clock, `agent.db` and the ledger.
///
/// Split out so the markup can be pinned by snapshot. `status_html`
/// otherwise reads the wall clock and two databases, which makes its
/// output — a precisely specified HTML fragment the UI injects with
/// `{@html}` — impossible to assert on. Same seam as
/// `decide_with_windows` and `is_fresh` elsewhere in this file.
pub struct StatusInputs {
    pub now_ms: i64,
    pub windows: BTreeMap<String, WindowState>,
    pub unaccounted_5h: f64,
    pub unaccounted_7d: f64,
    /// `limit_id` -> estimated window capacity in tokens, when known.
    pub capacities: BTreeMap<String, Option<f64>>,
    pub stale_after_s: f64,
}

/// Render the budget window bars (BACKEND-CONTRACT.md §2.3).
///
/// The class names are load-bearing against the `:global(.scv-*)` rules in
/// `hunter/ui-svelte/src/pages/StatusPage.svelte` -- not `hunter/ui/`, which
/// is generated build output -- and the UI injects this as raw HTML, so the
/// markup is the contract. A new class needs a rule there too. Pinned by
/// `tests/status_html_test.rs`.
#[allow(
    clippy::too_many_lines,
    reason = "one HTML template expressed as sequential `write!` calls; the \
              markup is the contract, so it has to be readable top to \
              bottom as the document it produces"
)]
#[must_use]
pub fn render_status(inputs: &StatusInputs) -> String {
    if inputs.windows.is_empty() {
        return NO_WINDOW_DATA.to_owned();
    }
    let mut fragments: Vec<String> = Vec::new();

    for (lid, w) in &inputs.windows {
        let label = format!("{} window", lid.replace("anthropic:", ""));

        let used_pct = match w.used_fraction {
            Some(f) => format!("{:.0}%", f * 100.0),
            None => "?".to_owned(),
        };

        // Dimension select.
        let (unacct, ramp_val, elapsed_frac): (f64, Option<f64>, Option<f64>) =
            if lid.contains(":5h") {
                let r = ramp_5h(w.resets_at, inputs.now_ms);
                let ef = match w.resets_at {
                    Some(ra) if ra > inputs.now_ms => {
                        Some((FIVE_H_MS as f64 - (ra - inputs.now_ms) as f64) / FIVE_H_MS as f64)
                    }
                    _ => None,
                };
                (inputs.unaccounted_5h, r, ef)
            } else if lid.contains(":7d") {
                (
                    inputs.unaccounted_7d,
                    Some(ramp_7d(w.resets_at, inputs.now_ms)),
                    None,
                )
            } else {
                (0.0, None, None)
            };

        // fill_pct: round-half-to-even (Python's round()), clamped 0..100.
        let fill_pct =
            ((w.used_fraction.unwrap_or(0.0) * 100.0).round_ties_even() as i64).clamp(0, 100) as u8;
        let soft_pct =
            ((unacct * 100.0).round_ties_even() as i64).clamp(0, 100 - i64::from(fill_pct)) as u8;
        let ramp_pct: Option<u8> =
            ramp_val.map(|r| ((r * 100.0).round_ties_even() as i64).clamp(0, 100) as u8);

        let ramp_for_avail = ramp_val.unwrap_or(1.0);
        let avail_frac = (ramp_for_avail - w.used_fraction.unwrap_or(0.0) - unacct).max(0.0);
        let avail_pct_str = format!("{:.0}%", avail_frac * 100.0);

        let cap = inputs.capacities.get(lid).copied().flatten();
        let avail_tok = cap.filter(|&c| c > 0.0).map(|c| avail_frac * c);

        let avail_str = if w.used_fraction.is_none() {
            String::new()
        } else if let Some(tok) = avail_tok {
            format!(
                " \u{00b7} {} avail (~{} tok)",
                avail_pct_str,
                fmt_tokens(tok)
            )
        } else {
            format!(" \u{00b7} {avail_pct_str} avail")
        };

        let is_stale = w.age_s > inputs.stale_after_s;
        let is_exhausted =
            w.status.as_deref() == Some("exhausted") || w.used_fraction.is_some_and(|f| f >= 1.0);
        let tone = if is_stale {
            "stale"
        } else if is_exhausted {
            "bad"
        } else {
            "ok"
        };

        let probe_age = format!("{:.0}m ago", w.age_s / 60.0);

        let reset_str = match w.resets_at {
            Some(r) => {
                let remain_s = (r - inputs.now_ms) as f64 / 1000.0;
                if remain_s > 0.0 {
                    let h = (remain_s / 3600.0) as i64;
                    let m = ((remain_s % 3600.0) / 60.0) as i64;
                    let countdown = if h > 0 {
                        format!("{h}h{m:02}m")
                    } else {
                        format!("{m}m")
                    };
                    // Emit <time data-ms="..."> so the client renders
                    // in the *browser's* local timezone, not the server's.
                    format!("resets <time data-ms=\"{r}\"></time> ({countdown})")
                } else {
                    "resetting".to_owned()
                }
            }
            None => "reset unknown".to_owned(),
        };

        let headroom_str = match (elapsed_frac, ramp_val) {
            (Some(ef), Some(r)) if r == 0.0 && ef > 0.0 => {
                let headroom_remain_ms = HEADROOM_MS as f64 - ef * FIVE_H_MS as f64;
                if headroom_remain_ms > 0.0 {
                    format!(
                        " \u{00b7} headroom {}m",
                        (headroom_remain_ms / 60_000.0) as i64
                    )
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };

        let unacct_str = if unacct > 0.005 {
            format!(" +{:.0}% in flight", unacct * 100.0)
        } else {
            String::new()
        };

        let marker = match ramp_pct {
            Some(rp) if rp > 0 => {
                format!(r#"<i class="scv-ramp" style="left:{rp}%"></i>"#)
            }
            _ => String::new(),
        };

        let stale_marker = if is_stale {
            " \u{26a0}\u{fe0f}stale"
        } else {
            ""
        };

        // Per-window fragment: exact HTML from BACKEND-CONTRACT.md §2.3.
        let fragment = format!(
            "<div class=\"scv-win\">\
                <div class=\"scv-lab\">\
                <b>{}</b>\
                <span>{} used{}{}{}</span>\
                </div>\
                <div class=\"scv-bar\">\
                <i class=\"scv-fill scv-{}\" style=\"width:{}%\"></i>\
                <i class=\"scv-soft\" style=\"width:{}%\"></i>\
                {}</div>\
                <div class=\"scv-sub\">{}{} \u{00b7} probed {}</div>\
                </div>",
            esc(&label),
            esc(&used_pct),
            esc(&unacct_str),
            esc(&avail_str),
            stale_marker,
            tone,
            fill_pct,
            soft_pct,
            marker,
            reset_str, // NOT escaped: contains <time data-ms="..."> for client-side TZ formatting
            esc(&headroom_str),
            esc(&probe_age),
        );

        fragments.push(fragment);
    }

    fragments.join("\n")
}

#[async_trait]
impl Backend for OmpScavengeBackend {
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let now = crate::util::now_ms();
        let db = self.agent_db.clone();
        let windows = tokio::task::spawn_blocking(move || capacity::read_windows(&db, now)).await?;
        self.decide_with_windows(&windows, anticipated_tokens).await
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        let now = crate::util::now_ms();
        let db = self.agent_db.clone();
        let windows = tokio::task::spawn_blocking(move || capacity::read_windows(&db, now)).await?;

        // _observe ALWAYS (observations + calibration logged even when fresh).
        self.observe(&windows).await?;

        // Staleness gate on anthropic:5h only: fresh → no probe.
        // Missing 5h (or no windows at all) → not fresh → probe.
        if self.is_fresh(&windows) {
            return Ok(false);
        }

        // Probe: invalidate then read (two spawn_blocking calls).
        {
            let omp = self.cfg.omp_bin.clone();
            let prober = self.prober.clone();
            tokio::task::spawn_blocking(move || {
                // rc ignored (best-effort cache bust).
                prober.run(
                    &[
                        omp.as_str(),
                        "usage",
                        "invalidate",
                        "--provider",
                        "anthropic",
                    ],
                    15,
                );
            })
            .await?;
        }

        let omp = self.cfg.omp_bin.clone();
        let prober = self.prober.clone();
        let (rc, _out) = tokio::task::spawn_blocking(move || {
            prober.run(&[omp.as_str(), "usage", "--provider", "anthropic"], 30)
        })
        .await?;

        Ok(rc == 0)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        let now_ms = crate::util::now_ms();
        let db = self.agent_db.clone();
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows(&db, now_ms)).await?;

        if windows.is_empty() {
            return Ok(NO_WINDOW_DATA.to_owned());
        }

        // anticipated=0: bars show observable state, not gate's hypothetical.
        let resv = self.unaccounted_fraction(&windows, 0).await?;

        let mut capacities = BTreeMap::new();
        for lid in windows.keys() {
            capacities.insert(lid.clone(), self.ledger.estimate_capacity(lid).await?);
        }

        Ok(render_status(&StatusInputs {
            now_ms,
            windows,
            unaccounted_5h: resv.gate_5h,
            unaccounted_7d: resv.gate_7d,
            capacities,
            stale_after_s: self.cfg.stale_after_s,
        }))
    }

    async fn run(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        cap_tokens: Option<i64>,
        max_wall_s: i64,
        job_class: JobClass,
    ) -> anyhow::Result<crate::types::RunResult> {
        // Usage-delta sandwich (`facade.OmpScavengeBackend.run`): snapshot the max 7d
        // used_fraction before and after run_worker.
        let pre = {
            let db = self.agent_db.clone();
            tokio::task::spawn_blocking(move || Self::_usage_snapshot_sync(&db)).await?
        };

        let model = self
            .cfg
            .model_for(job_class.as_str())
            .map(std::borrow::ToOwned::to_owned);
        let cfg = self.cfg.clone();
        let cwd_owned = cwd.to_owned();
        let prompt_owned = prompt.to_owned();
        let mut rr = tokio::task::spawn_blocking(move || {
            super::harness::run_worker(
                &cfg,
                &cwd_owned,
                &prompt_owned,
                cap_tokens,
                max_wall_s,
                model.as_deref(),
            )
        })
        .await?;

        let post = {
            let db = self.agent_db.clone();
            tokio::task::spawn_blocking(move || Self::_usage_snapshot_sync(&db)).await?
        };

        rr.usage_delta = match (pre, post) {
            (Some(p), Some(q)) => Some(q - p),
            _ => None,
        };
        Ok(rr)
    }
}
