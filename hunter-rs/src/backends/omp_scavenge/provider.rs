//! OMP LLM-provider quota definitions.
//!
//! A provider describes how OMP names and reports its usage windows.  The
//! scavenging policy itself remains in the shared two-window quota logic.

use super::capacity::{FIVE_H_MS, WEEK_MS, WindowState};

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

    pub const fn windows(self) -> ProviderWindows {
        match self {
            Self::Anthropic => ProviderWindows {
                short: ProviderWindow::new("anthropic:5h", FIVE_H_MS),
                long: ProviderWindow::new("anthropic:7d", WEEK_MS),
            },
            Self::OpenAiCodex => ProviderWindows {
                short: ProviderWindow::new("openai-codex:primary", FIVE_H_MS),
                long: ProviderWindow::new("openai-codex:secondary", WEEK_MS),
            },
        }
    }

    /// Period of `limit_id` when it names any provider's short or long
    /// window; None otherwise (Anthropic's per-model-class limits).
    /// Capacity calibration is keyed on this, and a daemon's window log
    /// may hold rows from whichever provider it ran with before.
    pub fn window_period(limit_id: &str) -> Option<i64> {
        Self::ALL.iter().find_map(|provider| {
            let limits = provider.windows();
            [limits.short, limits.long]
                .into_iter()
                .find(|window| window.limit_id == limit_id)
                .map(|window| window.period_ms)
        })
    }

    /// Whether `limit_id` is one of this provider's long windows. Anthropic
    /// also reports per-model-class weekly limits (`anthropic:7d:<class>`)
    /// and they gate like the account-wide one; Codex has exactly one.
    pub fn is_long_window(self, limit_id: &str) -> bool {
        match self {
            Self::Anthropic => limit_id.contains(":7d"),
            Self::OpenAiCodex => limit_id == self.windows().long.limit_id,
        }
    }

    /// Anthropic's `exhausted` status is a hard stop: the window counts as
    /// full and only its reset lifts the denial. Codex's statuses are
    /// derived from the reported fraction and add nothing to it.
    pub fn is_hard_stop(self, window: &WindowState) -> bool {
        matches!(self, Self::Anthropic) && window.status.as_deref() == Some("exhausted")
    }

    pub fn effective_used(self, window: &WindowState) -> Option<f64> {
        if self.is_hard_stop(window) {
            Some(1.0)
        } else {
            window.used_fraction
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderWindows {
    pub short: ProviderWindow,
    pub long: ProviderWindow,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderWindow {
    pub limit_id: &'static str,
    pub period_ms: i64,
}

impl ProviderWindow {
    pub const fn new(limit_id: &'static str, period_ms: i64) -> Self {
        Self {
            limit_id,
            period_ms,
        }
    }
}
