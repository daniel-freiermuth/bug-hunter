#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The UI has to be able to render every finding type the daemon emits.
//!
//! `FindingType` is serialized onto the wire and the Svelte UI switches on
//! those strings to pick a label and an icon. Nothing connects the two, so
//! adding a variant silently degrades the UI: the type pill falls back to
//! the raw wire name and the icon to a question mark. That is exactly what
//! happened when `standards` was added — five cases, six variants.
//!
//! This lives on the Rust side deliberately. The natural home looks like a
//! vitest case, but it needs to read a file, and the frontend's tsconfig
//! has no node types — the first attempt passed locally and failed
//! `svelte-check` in CI. Here, reading a sibling file is ordinary.

use std::path::Path;

use hunter::domain::FindingType;

fn format_ts() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hunter/ui-svelte/src/lib/format.ts");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Every wire name has an explicit case in both helpers.
#[test]
fn the_ui_renders_every_finding_type() {
    let src = format_ts();
    let (label_fn, rest) = src
        .split_once("export function typeEmoji")
        .expect("format.ts must define typeLabel then typeEmoji");

    let mut missing = Vec::new();
    for t in FindingType::ALL {
        let case = format!("case \"{}\":", t.as_str());
        if !label_fn.contains(&case) {
            missing.push(format!("typeLabel has no case for {:?}", t.as_str()));
        }
        if !rest.contains(&case) {
            missing.push(format!("typeEmoji has no case for {:?}", t.as_str()));
        }
    }
    assert!(
        missing.is_empty(),
        "hunter/ui-svelte/src/lib/format.ts is missing finding types: {missing:#?}"
    );
}

/// The fallback stays reachable. `Finding.type` is typed `string` in the
/// UI because it arrives as unvalidated JSON, so an older UI meeting a
/// newer daemon must degrade rather than break — which is also why the
/// compiler cannot enforce the check above.
#[test]
fn the_ui_keeps_a_fallback_for_unknown_types() {
    let src = format_ts();
    for helper in ["typeLabel", "typeEmoji"] {
        let start = src
            .find(&format!("export function {helper}"))
            .unwrap_or_else(|| panic!("format.ts must define {helper}"));
        let body = &src[start..];
        let end = body.find("\nexport ").unwrap_or(body.len());
        assert!(
            body[..end].contains("default:"),
            "{helper} must keep a default arm for a type this build has not heard of"
        );
    }
}
