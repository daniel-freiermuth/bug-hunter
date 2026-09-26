//! `OmpScavengeBackend` facade (facade.py, BACKEND-CONTRACT.md §2.2-2.3).
//! Bundles the harness, the window accounting and the scavenging policy
//! behind the single `Backend` the scheduler sees.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::capacity::{
    self, FIVE_H_MS, HEADROOM_MS, WEEK_MS, WindowState, ramp_5h, ramp_7d, retry_at_5h, retry_at_7d,
};
use super::provider::LlmProvider;

use crate::backend::SpendLedger;
use crate::backend::{Backend, JobClass, Outlook, Prober, Verdict};
use crate::config::Config;

/// 200 k ≈ 10% of a 5h window (`facade._TOK_PER_FRAC_5H`).
const TOK_PER_FRAC_5H: f64 = 2_000_000.0;
/// 5h / 7d period ratio ≈ 0.0297619.
const RATIO_5H_7D: f64 = FIVE_H_MS as f64 / WEEK_MS as f64;
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

        let limits = self.cfg.llm_provider.windows();
        let fallback = windows.values().map(|w| w.recorded_at).min().unwrap_or(0);
        let probe_at_5h = windows
            .get(limits.short.limit_id)
            .map_or(fallback, |w| w.recorded_at);
        let probe_at_7d = windows
            .get(limits.long.limit_id)
            .map_or(fallback, |w| w.recorded_at);

        let base = running + anticipated;
        let unaccounted_5h = base + self.ledger.finished_since(probe_at_5h).await?;
        let unaccounted_7d = base + self.ledger.finished_since(probe_at_7d).await?;

        let cap_5h = self
            .ledger
            .estimate_capacity(limits.short.limit_id)
            .await?
            .filter(|&v| v > 0.0)
            .unwrap_or(TOK_PER_FRAC_5H);
        let cap_7d = self
            .ledger
            .estimate_capacity(limits.long.limit_id)
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
        provider: LlmProvider,
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

        // Long-window pass: every long window the provider reports, which
        // for Anthropic includes the per-model-class weekly limits.
        let limits = provider.windows();
        for (lid, window) in windows {
            if !provider.is_long_window(lid) {
                continue;
            }
            if let Some(resets_at) = window.resets_at
                && resets_at > 0
                && resets_at <= now_ms
            {
                continue;
            }
            let Some(reported) = provider.effective_used(window) else {
                continue;
            };
            let effective_used = reported + resv.gate_7d;
            let allowed = ramp_7d(window.resets_at, now_ms);
            if effective_used < allowed {
                continue;
            }

            let exhausted = matches!(provider, LlmProvider::Anthropic)
                && window.status.as_deref() == Some("exhausted");
            let retry_at = if exhausted {
                window.resets_at.map(|reset| reset as f64)
            } else {
                retry_at_7d(window.resets_at, effective_used)
            };
            let reason = format!(
                "{lid}: used {reported:.2} + unaccounted {res:.2} = {effective_used:.2} >= ramp {allowed:.2}",
                res = resv.gate_7d
            );
            if prio && !exhausted {
                // Headroom for THIS job, so the reservation it is
                // measured against must not already contain it.
                let cap = Self::frac_to_tokens(
                    (1.0 - (reported + resv.budget_7d)).max(0.0),
                    "7d",
                    resv.cap_5h,
                    resv.cap_7d,
                );
                if cap > 0 {
                    return Verdict::Granted {
                        cap_tokens: Some(cap),
                        reason: format!("prio override ({reason})"),
                    };
                }
            }
            return Verdict::Denied { reason, retry_at };
        }

        // 5h pass.
        if let Some(w5) = windows.get(limits.short.limit_id)
            && let Some(reported) = provider.effective_used(w5)
            && let Some(allowed) = ramp_5h(w5.resets_at, now_ms)
        {
            let eff = reported + resv.gate_5h;
            if eff >= allowed {
                let is_exhausted = w5.status.as_deref() == Some("exhausted");
                let retry = if is_exhausted {
                    w5.resets_at.map(|r| r as f64)
                } else {
                    retry_at_5h(w5.resets_at, eff)
                };
                let reason = format!(
                    "5h: used {reported:.2} + unaccounted {res:.2} = {eff:.2} >= ramp {allowed:.2}",
                    res = resv.gate_5h
                );

                if prio && !is_exhausted {
                    // Same as the 7d arm: the cap is this job's budget.
                    let headroom_frac = (1.0 - (reported + resv.budget_5h)).max(0.0);
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
        let headroom = Self::compute_headroom(provider, windows, resv, prio, headroom_now);
        Verdict::Granted {
            cap_tokens: headroom,
            reason: "ok".to_owned(),
        }
    }

    /// Min headroom in tokens across windows
    /// (`facade.OmpScavengeBackend._compute_headroom`).
    ///
    /// Measured against `budget_*` (see [`Reservation`]): this headroom is
    /// what the anticipated job is allowed to spend, not what is left once
    /// it has spent it.
    fn compute_headroom(
        provider: LlmProvider,
        windows: &BTreeMap<String, WindowState>,
        resv: Reservation,
        prio: bool,
        now_ms: i64,
    ) -> Option<i64> {
        let mut caps: Vec<i64> = Vec::new();

        let limits = provider.windows();
        for (lid, window) in windows {
            let Some(used) = provider.effective_used(window) else {
                continue;
            };
            if provider.is_long_window(lid) {
                let ceiling = if prio {
                    1.0
                } else {
                    ramp_7d(window.resets_at, now_ms)
                };
                caps.push(Self::frac_to_tokens(
                    (ceiling - used - resv.budget_7d).max(0.0),
                    "7d",
                    resv.cap_5h,
                    resv.cap_7d,
                ));
            } else if lid == limits.short.limit_id
                && let Some(ceiling) = ramp_5h(window.resets_at, now_ms)
            {
                let ceiling = if prio { 1.0 } else { ceiling };
                caps.push(Self::frac_to_tokens(
                    (ceiling - used - resv.budget_5h).max(0.0),
                    "5h",
                    resv.cap_5h,
                    resv.cap_7d,
                ));
            }
        }

        caps.iter().copied().min()
    }

    /// Log observations and calibrate the selected provider's two windows.
    async fn observe(&self, windows: &BTreeMap<String, WindowState>) -> anyhow::Result<()> {
        let now = crate::util::now_ms();
        let limits = self.cfg.llm_provider.windows();

        for window in windows.values() {
            let horizon = if window.limit_id == limits.short.limit_id {
                Some(limits.short.period_ms)
            } else if window.limit_id == limits.long.limit_id {
                Some(limits.long.period_ms)
            } else {
                None
            };

            if let Some(period_ms) = horizon
                && let (Some(resets), Some(used_fraction)) =
                    (window.resets_at, window.used_fraction)
                && let Some((previous_observation, previous_fraction)) = self
                    .ledger
                    .last_window_observation(&window.limit_id, resets)
                    .await?
                && used_fraction > previous_fraction
                && (now - previous_observation) <= period_ms
            {
                let tokens = self
                    .ledger
                    .finished_between(previous_observation, now)
                    .await?;
                if tokens > 0 {
                    self.ledger
                        .record_calibration_sample(
                            &window.limit_id,
                            Some(resets),
                            used_fraction - previous_fraction,
                            tokens,
                        )
                        .await?;
                }
            }

            self.ledger
                .log_window_observation(
                    &window.limit_id,
                    window.used_fraction,
                    window.status.as_deref(),
                    window.resets_at,
                    window.age_s,
                )
                .await?;
        }

        Ok(())
    }

    /// Max `used_fraction` across long windows, or None (`facade.OmpScavengeBackend._usage_snapshot`).
    /// Blocking — meant for `spawn_blocking`.
    fn _usage_snapshot_sync(agent_db: &std::path::Path, provider: LlmProvider) -> Option<f64> {
        let now = crate::util::now_ms();
        let windows = capacity::read_windows_for(agent_db, provider, now);
        windows
            .iter()
            .filter(|(lid, _)| provider.is_long_window(lid))
            .filter_map(|(_, w)| w.used_fraction)
            .reduce(f64::max)
    }
}

// ---- public testing seam --------------------------------------------------

impl OmpScavengeBackend {
    /// Testing seam: runs decide logic on pre-built windows (skips
    /// `read_windows`).  Not part of the Backend trait.
    #[allow(clippy::similar_names)]
    pub async fn decide_with_windows(
        &self,
        windows: &BTreeMap<String, WindowState>,
        anticipated_tokens: i64,
    ) -> anyhow::Result<Outlook> {
        let now = crate::util::now_ms();
        let resv = self
            .unaccounted_fraction(windows, anticipated_tokens)
            .await?;
        let normal = Self::decide_inner(self.cfg.llm_provider, windows, resv, false, now);

        let prioritized = match &normal {
            Verdict::Granted {
                cap_tokens: normal_cap,
                ..
            } => {
                // Normal granted → prioritized = normal, with possible
                // prio-headroom upgrade (fresh now_ms).
                let prio_now = crate::util::now_ms();
                let prio_headroom =
                    Self::compute_headroom(self.cfg.llm_provider, windows, resv, true, prio_now);
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
                Self::decide_inner(self.cfg.llm_provider, windows, resv, true, now)
            }
        };

        Ok(Outlook {
            normal,
            prioritized,
        })
    }

    /// Staleness predicate behind `keep_fresh`'s probe gate: fresh iff the
    /// provider's short window exists and its age is within `stale_after_s`,
    /// inclusive — an age landing exactly on the threshold is still fresh.
    /// A missing short window (or no windows at all) is not fresh.
    ///
    /// Split out from `keep_fresh` so the boundary is observable without
    /// the wall-clock read that derives `age_s`.
    pub fn is_fresh(&self, windows: &BTreeMap<String, WindowState>) -> bool {
        windows
            .get(self.cfg.llm_provider.windows().short.limit_id)
            .is_some_and(|short| short.age_s <= self.cfg.stale_after_s)
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
    pub provider: LlmProvider,
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

    let limits = inputs.provider.windows();
    for (lid, w) in &inputs.windows {
        let label = if lid == limits.short.limit_id {
            "5h window".to_owned()
        } else if lid == limits.long.limit_id {
            "7d window".to_owned()
        } else {
            format!("{} window", lid.replace("anthropic:", ""))
        };
        let used_pct = match w.used_fraction {
            Some(f) => format!("{:.0}%", f * 100.0),
            None => "?".to_owned(),
        };

        let (unacct, ramp_val, elapsed_frac): (f64, Option<f64>, Option<f64>) =
            if lid == limits.short.limit_id {
                let ramp = ramp_5h(w.resets_at, inputs.now_ms);
                // A ramp exists only while the window is active, i.e.
                // `resets_at` is in the future.
                let elapsed = ramp.and(w.resets_at).map(|reset| {
                    (FIVE_H_MS as f64 - (reset - inputs.now_ms) as f64) / FIVE_H_MS as f64
                });
                (inputs.unaccounted_5h, ramp, elapsed)
            } else if inputs.provider.is_long_window(lid) {
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
        let provider = self.cfg.llm_provider;
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows_for(&db, provider, now))
                .await?;
        self.decide_with_windows(&windows, anticipated_tokens).await
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        let now = crate::util::now_ms();
        let db = self.agent_db.clone();
        let provider = self.cfg.llm_provider;
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows_for(&db, provider, now))
                .await?;

        self.observe(&windows).await?;
        if self.is_fresh(&windows) {
            return Ok(false);
        }

        {
            let omp = self.cfg.omp_bin.clone();
            let prober = self.prober.clone();
            let provider_name = self.cfg.llm_provider.name();
            tokio::task::spawn_blocking(move || {
                prober.run(
                    &[
                        omp.as_str(),
                        "usage",
                        "invalidate",
                        "--provider",
                        provider_name,
                    ],
                    15,
                );
            })
            .await?;
        }

        let omp = self.cfg.omp_bin.clone();
        let prober = self.prober.clone();
        let provider_name = self.cfg.llm_provider.name();
        let (rc, _out) = tokio::task::spawn_blocking(move || {
            prober.run(&[omp.as_str(), "usage", "--provider", provider_name], 30)
        })
        .await?;

        Ok(rc == 0)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        let now_ms = crate::util::now_ms();
        let db = self.agent_db.clone();
        let provider = self.cfg.llm_provider;
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows_for(&db, provider, now_ms))
                .await?;

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
            provider: self.cfg.llm_provider,
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
        cap_tokens: i64,
        max_wall_s: i64,
        job_class: JobClass,
    ) -> anyhow::Result<crate::types::RunResult> {
        let pre = {
            let db = self.agent_db.clone();
            let provider = self.cfg.llm_provider;
            tokio::task::spawn_blocking(move || Self::_usage_snapshot_sync(&db, provider)).await?
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
            let provider = self.cfg.llm_provider;
            tokio::task::spawn_blocking(move || Self::_usage_snapshot_sync(&db, provider)).await?
        };

        rr.usage_delta = match (pre, post) {
            (Some(before), Some(after)) => Some(after - before),
            _ => None,
        };
        Ok(rr)
    }
}
