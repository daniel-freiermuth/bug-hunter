#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Property tests over the functions whose bugs were all "an input class
//! nobody thought of".
//!
//! Every regression across three review rounds was a specific unconsidered
//! input: a value containing the delimiter, bytes that are not UTF-8, a
//! truncation landing mid-codepoint. Example-based tests only cover the
//! cases someone imagined, which is exactly the faculty that kept failing.
//! These state the invariant and let proptest search for the counterexample.

use proptest::prelude::*;

use hunter::playbooks::render;
use hunter::util::tail;

proptest! {
    /// `tail` is used at ~20 call sites on command output and worker logs.
    /// Byte-indexing it was a live panic; these are the properties that
    /// make the whole class unreachable rather than the one case we hit.
    #[test]
    fn tail_is_always_a_valid_suffix(s: String, n in 0usize..512) {
        let got = tail(&s, n);
        prop_assert!(s.ends_with(got), "not a suffix of the input");
        prop_assert!(got.len() <= s.len());
        // Snapping forward to a char boundary can only ever shorten it,
        // so the limit is an upper bound.
        prop_assert!(got.len() <= n);
    }

    #[test]
    fn tail_keeps_everything_when_the_limit_exceeds_the_input(s: String) {
        prop_assert_eq!(tail(&s, s.len() + 1), s.as_str());
    }

    /// The `render` bug in full: a PR body, repo note or finding summary
    /// containing braces must not be mistaken for an unfilled placeholder.
    /// The template here is fully satisfied, so rendering must succeed no
    /// matter what the value contains.
    #[test]
    fn render_never_rejects_a_template_whose_slots_are_all_supplied(value: String) {
        let slots = std::collections::HashMap::from([("BODY", value.clone())]);
        let out = render("before {{BODY}} after", &slots);
        prop_assert!(out.is_ok(), "rejected a supplied slot: {:?}", out.err());
        prop_assert_eq!(out.unwrap(), format!("before {value} after"));
    }

    /// A value is data, never template. Whatever it contains, it appears
    /// verbatim and is not rescanned for slots — the structural reason
    /// template injection is impossible rather than escaped away.
    #[test]
    fn render_never_reinterprets_a_substituted_value(value: String, other: String) {
        let slots = std::collections::HashMap::from([
            ("A", value.clone()),
            ("B", other.clone()),
        ]);
        let out = render("{{A}}|{{B}}", &slots).expect("both slots supplied");
        prop_assert_eq!(out, format!("{value}|{other}"));
    }
}
