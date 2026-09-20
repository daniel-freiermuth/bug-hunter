#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `render_status` — the budget window bars (BACKEND-CONTRACT.md §2.3).
//!
//! This markup is not an API, it is presentation: the UI injects it with
//! `{@html store.summary.backend_status_html}`, and every class name is
//! load-bearing against the `:global(.scv-*)` rules in
//! `hunter/ui-svelte/src/pages/StatusPage.svelte` (`.scv-win`,
//! `.scv-fill`, `.scv-ok`/`.scv-bad`/`.scv-stale`, `.scv-soft`,
//! `.scv-ramp`, `.scv-sub`). Not `hunter/ui/index.html`: that is the
//! generated Vite bundle, gitignored, so a rename chased there would
//! be edited in a build artefact and lost on the next build. The contract specifies it to
//! the character, down to the MIDDLE DOT separators and which fields are
//! HTML-escaped.
//!
//! Snapshots are the right shape for it: the contract is written in
//! HTML, so the diff should be too, and ~115 lines of specified
//! rendering are worth more than a handful of hand-picked assertions.
//!
//! Deterministic by construction: `render_status` takes the clock and
//! every database-derived value as input, and the reset time is emitted
//! as `<time data-ms>` for the browser to format, so no server-local
//! 12-hour clock can leak a machine-dependent string into a snapshot.

use std::collections::BTreeMap;

use hunter::backends::omp_scavenge::capacity::WindowState;
use hunter::backends::omp_scavenge::{StatusInputs, render_status};

/// Fixed instant so `resets_at` offsets below read as durations.
const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn window(
    limit_id: &str,
    used: Option<f64>,
    status: &str,
    resets_in: Option<i64>,
    age_s: f64,
) -> WindowState {
    WindowState {
        limit_id: limit_id.to_owned(),
        used_fraction: used,
        status: Some(status.to_owned()),
        resets_at: resets_in.map(|d| NOW + d),
        recorded_at: NOW - (age_s * 1000.0) as i64,
        age_s,
    }
}

struct Case {
    windows: Vec<WindowState>,
    unaccounted_5h: f64,
    unaccounted_7d: f64,
    capacity: Option<f64>,
}

impl Case {
    fn new(windows: Vec<WindowState>) -> Self {
        Self {
            windows,
            unaccounted_5h: 0.0,
            unaccounted_7d: 0.0,
            capacity: Some(3_000_000.0),
        }
    }

    fn render(self) -> String {
        let mut capacities = BTreeMap::new();
        let mut map = BTreeMap::new();
        for w in self.windows {
            capacities.insert(w.limit_id.clone(), self.capacity);
            map.insert(w.limit_id.clone(), w);
        }
        render_status(&StatusInputs {
            now_ms: NOW,
            windows: map,
            unaccounted_5h: self.unaccounted_5h,
            unaccounted_7d: self.unaccounted_7d,
            capacities,
            stale_after_s: 300.0,
        })
    }
}

/// §2.3 first line: no windows at all returns exactly this string.
#[test]
fn no_window_data() {
    insta::assert_snapshot!(Case::new(vec![]).render());
}

/// The ordinary case: both dimensions, mid-window, freshly probed.
#[test]
fn fresh_both_windows() {
    insta::assert_snapshot!(
        Case::new(vec![
            window("anthropic:5h", Some(0.32), "ok", Some(2 * HOUR), 45.0),
            window("anthropic:7d", Some(0.43), "ok", Some(64 * HOUR), 90.0),
        ])
        .render()
    );
}

/// In-flight spend the probe cannot see yet: adds the `+N% in flight`
/// note and the striped `.scv-soft` overlay beside the fill.
#[test]
fn unaccounted_spend_in_flight() {
    let mut case = Case::new(vec![window(
        "anthropic:5h",
        Some(0.32),
        "ok",
        Some(2 * HOUR),
        45.0,
    )]);
    case.unaccounted_5h = 0.10;
    insta::assert_snapshot!(case.render());
}

/// Stale beats exhausted: a window past `stale_after_s` renders the
/// `stale` tone and the warning marker even when it is also full.
#[test]
fn stale_wins_over_exhausted() {
    insta::assert_snapshot!(
        Case::new(vec![window(
            "anthropic:5h",
            Some(1.0),
            "exhausted",
            Some(HOUR),
            9_000.0
        )])
        .render()
    );
}

/// Exhausted while fresh: `bad` tone, no warning marker.
#[test]
fn exhausted_but_fresh() {
    insta::assert_snapshot!(
        Case::new(vec![window(
            "anthropic:7d",
            Some(1.0),
            "exhausted",
            Some(24 * HOUR),
            30.0
        )])
        .render()
    );
}

/// Inside the human headroom the 5h ramp is still zero, so the ramp
/// marker is suppressed (it would sit invisibly at 0%) and a
/// `headroom Nm` note appears instead.
#[test]
fn inside_the_headroom_window() {
    // 10 minutes into a 5h window: ramp has not opened yet.
    insta::assert_snapshot!(
        Case::new(vec![window(
            "anthropic:5h",
            Some(0.02),
            "ok",
            Some(5 * HOUR - 10 * 60 * 1000),
            20.0
        )])
        .render()
    );
}

/// No `used_fraction` yet: `?` for the percentage and the availability
/// note is suppressed entirely rather than rendered as `0%`.
#[test]
fn unknown_used_fraction() {
    insta::assert_snapshot!(
        Case::new(vec![window(
            "anthropic:5h",
            None,
            "ok",
            Some(3 * HOUR),
            12.0
        )])
        .render()
    );
}

/// A per-model-class dimension still takes the 7d branch — the select
/// is a SUBSTRING test, and `":7d" in "anthropic:7d:model-class"` holds
/// in both the Python original and the port — so it does get a ramp.
/// What it loses is the token annotation: `estimate_capacity` is keyed
/// on the full limit id and has no calibration for a model-class
/// dimension, so the availability note renders as a bare percentage.
#[test]
fn per_model_class_window() {
    let mut case = Case::new(vec![window(
        "anthropic:7d:model-class",
        Some(0.21),
        "ok",
        Some(48 * HOUR),
        60.0,
    )]);
    case.capacity = None;
    insta::assert_snapshot!(case.render());
}

/// The label and the escaped fields go through html escaping; the
/// `<time data-ms>` element must NOT, or the browser sees text.
#[test]
fn limit_id_is_escaped_but_the_time_element_is_not() {
    let out = Case::new(vec![window(
        "anthropic:<script>",
        Some(0.5),
        "ok",
        Some(HOUR),
        10.0,
    )])
    .render();
    assert!(
        out.contains("&lt;script&gt; window"),
        "label must be escaped: {out}"
    );
    assert!(
        out.contains("<time data-ms="),
        "reset time must stay raw markup: {out}"
    );
    insta::assert_snapshot!(out);
}

/// Python's `round()` is round-half-to-even and Rust's `f64::round()` is
/// half-away-from-zero, so the contract calls for `round_ties_even`. At
/// exactly 36.5% that is the difference between `36%` and `37%` in a
/// number a human reads off the bar.
#[test]
fn fill_percent_rounds_half_to_even() {
    let out = Case::new(vec![window(
        "anthropic:5h",
        Some(0.365),
        "ok",
        Some(HOUR),
        10.0,
    )])
    .render();
    // 0.365 itself is not exactly representable, but the scaled product is:
    // the nearest double to 0.365 (0x3FD75C28F5C28F5C) times 100.0 lands on
    // exactly 36.5 (0x4042400000000000, fract() == 0.5 with zero error), so
    // this input is a genuine tie and not merely a value that looks like one.
    // That exactness is the whole point of picking it: `round()` answers 37
    // and `round_ties_even()` answers 36, so swapping the production call to
    // `round()` fails here and nowhere else in the suite. Every other
    // used_fraction in these tests (0.02, 0.21, 0.32, 0.43, 0.5) scales to a
    // non-tie, which no rounding mode can tell apart.
    assert!(
        out.contains(r#"style="width:36%""#),
        "0.365 must round to 36 (half-to-even), not 37: {out}"
    );
}
