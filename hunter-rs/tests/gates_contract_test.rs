#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `just lint` and `just test` must run what CI runs.
//!
//! A local gate weaker than the remote one is worse than no local gate:
//! it answers the question you asked it, wrongly, and the failure shows
//! up on the push instead. All three checks had drifted at once —
//! `lint` omitted `cargo fmt --check` entirely and ran clippy without
//! `-D warnings` (Cargo.toml sets `pedantic = "warn"`, so a pedantic
//! finding left the recipe at exit 0 while CI failed), and `test` ran
//! the default nextest profile, which stops at the first failure and
//! kills a test at 60s where CI reports everything and allows 120s.
//!
//! So this asserts the direction that matters — every command in CI's
//! Rust job is in the justfile — and deliberately not the reverse: the
//! justfile is also a developer's toolbox (`run`, `dev-db`, `mutants`),
//! and requiring CI to carry those would be requiring the wrong thing.
//!
//! It is a substring check over two text files, which is crude, but the
//! alternative is parsing YAML and a just grammar to defend a property
//! whose whole content is "these strings are equal".

use std::path::Path;

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The justfile's executable lines, comments stripped.
///
/// Whole-file matching is not good enough, and this is not theoretical:
/// the first version of this test passed while `cargo fmt --check` was
/// deleted from the recipe, because the comment above the recipe quotes
/// the command. A gate test satisfied by prose about the gate is worse
/// than no test.
fn justfile_commands() -> String {
    read("justfile")
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `run:` lines of CI's `Rust — lint + test` job, in order.
///
/// Scoped to that one job: the coverage job runs `cargo llvm-cov`, which
/// needs a toolchain component and minutes, and is deliberately not a
/// local gate.
fn ci_rust_commands() -> Vec<String> {
    let ci = read("../.github/workflows/ci.yml");
    let start = ci
        .find("name: Rust — lint + test")
        .expect("ci.yml must have a job named 'Rust — lint + test'");
    let body = &ci[start..];
    // Job boundary: the next line at the job-name indent (4 spaces).
    let end = body[1..].find("\n    name:").map_or(body.len(), |i| i + 1);

    body[..end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix("run: "))
        .map(str::trim)
        // Setup steps (`rustup component add`, action shims) are not gates.
        .filter(|c| c.starts_with("cargo ") || c.starts_with("./"))
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn the_justfile_runs_every_gate_ci_runs() {
    let justfile = justfile_commands();
    let ci = ci_rust_commands();

    assert!(
        ci.len() >= 4,
        "expected to find CI's fmt/clippy/sql/test commands, got {ci:#?} — \
         the job name or `run:` shape in ci.yml probably changed, and this \
         test is now checking nothing"
    );

    let missing: Vec<&String> = ci.iter().filter(|c| !justfile.contains(*c)).collect();
    assert!(
        missing.is_empty(),
        "hunter-rs/justfile does not run these commands from CI's Rust job: {missing:#?}\n\
         A recipe that runs a weaker check than CI reports clean on a tree CI rejects."
    );
}

/// The flags that carry the weight, named individually.
///
/// The check above passes if someone weakens CI to match a weakened
/// justfile. These two are why the gates exist at all.
#[test]
fn both_gates_keep_the_flags_that_make_them_strict() {
    let justfile = justfile_commands();
    let ci = read("../.github/workflows/ci.yml");

    for (file, src) in [("justfile", &justfile), ("ci.yml", &ci)] {
        assert!(
            src.contains("cargo clippy --all-targets -- -D warnings"),
            "{file} must deny clippy warnings: pedantic is `warn` in Cargo.toml, \
             so without -D warnings a pedantic finding passes silently"
        );
        assert!(
            src.contains("cargo nextest run --profile ci"),
            "{file} must run the ci nextest profile: the default profile stops at \
             the first failure and kills a test at 60s where ci allows 120s"
        );
    }
}
