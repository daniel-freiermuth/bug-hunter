//! OMP LLM-provider quota definitions.
//!
//! A provider is data: how OMP names its usage limits and which quota
//! windows gate spending. The scavenging policy in `facade` loops over
//! [`Quota::windows`] and holds no provider- or window-specific branch.

use super::capacity::{
    FIVE_H_MS, WEEK_MS, WindowState, ramp_after_headroom, ramp_linear, retry_at_after_headroom,
    retry_at_linear,
};

/// Capacity of a 5h window before calibration history exists: 200 k ≈
/// 10% of it (`facade._TOK_PER_FRAC_5H`).
const FIVE_H_FALLBACK_TOKENS: f64 = 2_000_000.0;
/// 5h / 7d period ratio ≈ 0.0297619.
const RATIO_5H_7D: f64 = FIVE_H_MS as f64 / WEEK_MS as f64;

static ANTHROPIC: Quota = Quota {
    windows: &[
        QuotaWindow {
            limit_id: "anthropic:7d",
            label: "7d window",
            period_ms: WEEK_MS,
            pacing: Pacing::Linear,
            extra_prefix: Some("anthropic:7d:"),
            fallback_capacity: FallbackCapacity::Scaled {
                window: 1,
                ratio: RATIO_5H_7D,
            },
        },
        QuotaWindow {
            limit_id: "anthropic:5h",
            label: "5h window",
            period_ms: FIVE_H_MS,
            pacing: Pacing::AfterHeadroom,
            extra_prefix: None,
            fallback_capacity: FallbackCapacity::Tokens(FIVE_H_FALLBACK_TOKENS),
        },
    ],
    exhausted_is_hard_stop: true,
};

static OPENAI_CODEX: Quota = Quota {
    windows: &[
        QuotaWindow {
            limit_id: "openai-codex:secondary",
            label: "7d window",
            period_ms: WEEK_MS,
            pacing: Pacing::Linear,
            extra_prefix: None,
            fallback_capacity: FallbackCapacity::Scaled {
                window: 1,
                ratio: RATIO_5H_7D,
            },
        },
        QuotaWindow {
            limit_id: "openai-codex:primary",
            label: "5h window",
            period_ms: FIVE_H_MS,
            pacing: Pacing::AfterHeadroom,
            extra_prefix: None,
            fallback_capacity: FallbackCapacity::Tokens(FIVE_H_FALLBACK_TOKENS),
        },
    ],
    // Codex's statuses are derived from the reported fraction and add
    // nothing to it.
    exhausted_is_hard_stop: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Anthropic,
    OpenAiCodex,
}

impl LlmProvider {
    pub const ALL: [Self; 2] = [Self::Anthropic, Self::OpenAiCodex];

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.name() == value)
    }

    /// Every accepted name, quoted, for error messages:
    /// `'anthropic' or 'openai-codex'`.
    pub fn expected_names() -> String {
        Self::ALL
            .map(|provider| format!("'{}'", provider.name()))
            .join(" or ")
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCodex => "openai-codex",
        }
    }

    pub fn quota(self) -> &'static Quota {
        match self {
            Self::Anthropic => &ANTHROPIC,
            Self::OpenAiCodex => &OPENAI_CODEX,
        }
    }

    /// Period of `limit_id` when it names any provider's quota window;
    /// None otherwise (Anthropic's per-model-class limits). Capacity
    /// calibration is keyed on this, and a daemon's window log may hold
    /// rows from whichever provider it ran with before.
    pub fn window_period(limit_id: &str) -> Option<i64> {
        Self::ALL
            .iter()
            .find_map(|provider| provider.quota().window(limit_id))
            .map(|window| window.period_ms)
    }
}

/// The windows that gate one provider's spending.
#[derive(Debug)]
pub struct Quota {
    /// Longest period first. The order is policy: the first window that
    /// denies supplies the reason and `retry_at`, the longest window's
    /// fraction is the usage delta, and the shortest window's probe age
    /// decides whether omp must be re-probed.
    pub windows: &'static [QuotaWindow],
    /// Whether an `exhausted` status is a hard stop: the window counts as
    /// full and only its reset lifts the denial, even for prioritized work.
    pub exhausted_is_hard_stop: bool,
}

impl Quota {
    /// The quota window whose account-wide row is `limit_id`.
    pub fn window(&self, limit_id: &str) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| window.limit_id == limit_id)
    }

    /// The quota window `limit_id` gates as: its account-wide row or one of
    /// its extra limits. None for limits that are only displayed.
    pub fn gating_window(&self, limit_id: &str) -> Option<&QuotaWindow> {
        self.windows.iter().find(|window| window.gates(limit_id))
    }

    pub fn is_hard_stop(&self, window: &WindowState) -> bool {
        self.exhausted_is_hard_stop && window.status.as_deref() == Some("exhausted")
    }

    pub fn effective_used(&self, window: &WindowState) -> Option<f64> {
        if self.is_hard_stop(window) {
            Some(1.0)
        } else {
            window.used_fraction
        }
    }

    /// Token capacity of `windows[index]`: its calibrated `estimate`
    /// when there is one, else its fallback.
    pub fn capacity(&self, index: usize, estimates: &[Option<f64>]) -> f64 {
        let estimate = estimates.get(index).copied().flatten();
        let fallback = || match self
            .windows
            .get(index)
            .map(|window| window.fallback_capacity)
        {
            Some(FallbackCapacity::Tokens(tokens)) => tokens,
            Some(FallbackCapacity::Scaled { window, ratio }) => {
                self.capacity(window, estimates) / ratio
            }
            None => 0.0,
        };
        estimate.unwrap_or_else(fallback)
    }
}

/// How a window's allowance grows over its cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// [`ramp_linear`]: the elapsed fraction of the cycle.
    Linear,
    /// [`ramp_after_headroom`]: nothing for the first `HEADROOM_MS`, then
    /// linear. Without an active cycle the window does not gate.
    AfterHeadroom,
}

/// A window's token capacity before calibration history exists.
#[derive(Debug, Clone, Copy)]
pub enum FallbackCapacity {
    Tokens(f64),
    /// `windows[window]`'s capacity divided by `ratio` (its period over
    /// this one's), so a calibrated short window scales up.
    Scaled {
        window: usize,
        ratio: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaWindow {
    /// The account-wide row: calibration, reservation and freshness key.
    pub limit_id: &'static str,
    /// Status-page label of the account-wide row.
    pub label: &'static str,
    pub period_ms: i64,
    pub pacing: Pacing,
    /// Extra limits `<prefix><class>` that gate like this window, against
    /// its reservation and capacity (Anthropic's per-model-class limits).
    pub extra_prefix: Option<&'static str>,
    pub fallback_capacity: FallbackCapacity,
}

impl QuotaWindow {
    pub fn gates(&self, limit_id: &str) -> bool {
        limit_id == self.limit_id
            || self
                .extra_prefix
                .is_some_and(|prefix| limit_id.starts_with(prefix))
    }

    /// Allowed used fraction at `now_ms`; None when the window is not
    /// active and therefore does not gate.
    pub fn ramp(&self, resets_at: Option<i64>, now_ms: i64) -> Option<f64> {
        match self.pacing {
            Pacing::Linear => Some(ramp_linear(resets_at, self.period_ms, now_ms)),
            Pacing::AfterHeadroom => ramp_after_headroom(resets_at, self.period_ms, now_ms),
        }
    }

    /// When the ramp reaches `effective_used`: the inverse of [`Self::ramp`].
    pub fn retry_at(&self, resets_at: Option<i64>, effective_used: f64) -> Option<f64> {
        match self.pacing {
            Pacing::Linear => retry_at_linear(resets_at, self.period_ms, effective_used),
            Pacing::AfterHeadroom => {
                retry_at_after_headroom(resets_at, self.period_ms, effective_used)
            }
        }
    }
}
