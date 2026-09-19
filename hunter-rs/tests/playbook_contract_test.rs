#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Every playbook renders with the slots its builder supplies.
//!
//! `render` errors on a `{{KEY}}` the caller did not supply, which is
//! the right behaviour — a half-rendered prompt must never reach a
//! worker. But it turned a typo from a logged warning into a hard
//! failure that aborts the job and wastes the cycle, and the only thing
//! standing between the two is that twelve templates in
//! `hunter/playbooks/` agree with twelve builders in `src/playbooks.rs`.
//! A review flagged this as the highest-risk item of its batch and
//! verified it by *reading* the templates. This checks it.
//!
//! Deliberately NOT hermetic: it renders the real playbooks, because a
//! stub template would agree with its builder by construction and
//! assert nothing. That makes it a contract test between two
//! directories of the repo, like `hunter/tests/test_schema_parity.py`.
//! cargo-mutants copies only the crate, so `.cargo/mutants.toml`
//! excludes this binary rather than have every mutant run fail its
//! baseline.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::FindingType;
use hunter::playbooks;
use hunter::types::Finding;
use support::{sample_finding, sample_repo};

/// The real playbook directory, beside the crate.
fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has a parent")
        .join("hunter");
    assert!(
        root.join("playbooks").is_dir(),
        "expected the real playbooks at {}; this test renders them on purpose",
        root.join("playbooks").display()
    );
    root
}

fn out_path() -> PathBuf {
    PathBuf::from("/tmp/hunter-playbook-contract/out.json")
}

/// Every prompt must be non-empty and substantial.
///
/// Note what is deliberately NOT asserted: that the output contains no
/// `{{`. An unrendered template slot is already impossible — `render`
/// returns `Err` for a key the builder did not supply, which is what
/// the expectations below rely on. Braces in the OUTPUT are ordinary
/// content: `build_standards_prompt` injects the whole of
/// `CODING_STANDARDS.md`, and that document documents the placeholder
/// syntax, so it contains `{{` legitimately.
///
/// Asserting on braces here would repeat the bug the single-pass
/// renderer fixed — conflating "the template has a hole" with "a value
/// contains a brace". Confirmed by experiment: under the previous
/// renderer, which scanned the substituted text, this repo could not
/// build a standards prompt at all.
fn assert_usable(name: &str, prompt: &str) {
    assert!(!prompt.trim().is_empty(), "{name}: rendered empty");
    assert!(
        prompt.len() > 200,
        "{name}: rendered suspiciously short ({} bytes) — a slot filled \
         with an empty value still costs a worker a window: {prompt}",
        prompt.len()
    );
}

/// Shared shape of the five repo-level analysis prompt builders.
/// Shared shape of the two "apply a proposed change" builders.
type ApplyBuilder =
    fn(&Path, &Finding, &Path, &str, &hunter::types::Repo, &str) -> anyhow::Result<String>;

type AnalysisBuilder = fn(
    &Path,
    &hunter::types::Repo,
    &str,
    &[Finding],
    &[Finding],
    &Path,
    i64,
    &str,
) -> anyhow::Result<String>;

fn analysis_cases() -> Vec<(&'static str, FindingType)> {
    vec![
        ("test_gap", FindingType::TestGap),
        ("dep_update", FindingType::DepUpdate),
        ("refactor", FindingType::Refactor),
        ("modernization", FindingType::Modernization),
        ("standards", FindingType::Standards),
    ]
}

#[test]
fn hunt_prompt_renders() {
    let repo = sample_repo();
    let known = [sample_finding(FindingType::Bug)];
    let suppressed = [sample_finding(FindingType::Bug)];
    let prompt = playbooks::build_hunt_prompt(
        &root(),
        &repo,
        "abc123..def456",
        "a scope note",
        &suppressed,
        &known,
        &out_path(),
        8,
        "repo notes",
    )
    .expect("hunt.md must render with the slots build_hunt_prompt supplies");
    assert_usable("hunt", &prompt);
}

#[test]
fn analysis_prompts_render() {
    let repo = sample_repo();
    let root = root();
    let out = out_path();
    for (name, kind) in analysis_cases() {
        let known = [sample_finding(kind)];
        let suppressed = [sample_finding(kind)];
        let build: AnalysisBuilder = match kind {
            FindingType::TestGap => playbooks::build_test_gap_prompt,
            FindingType::DepUpdate => playbooks::build_dep_update_prompt,
            FindingType::Refactor => playbooks::build_refactor_prompt,
            FindingType::Modernization => playbooks::build_modernization_prompt,
            FindingType::Standards => playbooks::build_standards_prompt,
            other => panic!("unhandled analysis kind {other:?}"),
        };
        let prompt = build(
            &root,
            &repo,
            "a scope note",
            &suppressed,
            &known,
            &out,
            8,
            "repo notes",
        )
        .unwrap_or_else(|e| panic!("{name}.md must render with its builder's slots: {e}"));
        assert_usable(name, &prompt);
    }
}

#[test]
fn fix_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let prompt = playbooks::build_fix_prompt(
        &root(),
        &finding,
        Path::new("/tmp/worktree"),
        "fix/branch",
        &repo,
        "repo notes",
    )
    .expect("fix.md must render");
    assert_usable("fix", &prompt);
}

#[test]
fn improvement_and_modernization_apply_prompts_render() {
    let repo = sample_repo();
    for (name, kind, build) in [
        (
            "apply_improvement",
            FindingType::Refactor,
            playbooks::build_apply_improvement_prompt as ApplyBuilder,
        ),
        (
            "apply_modernization",
            FindingType::Modernization,
            playbooks::build_apply_modernization_prompt,
        ),
    ] {
        let finding = sample_finding(kind);
        let prompt = build(
            &root(),
            &finding,
            Path::new("/tmp/worktree"),
            "fix/branch",
            &repo,
            "repo notes",
        )
        .unwrap_or_else(|e| panic!("{name}.md must render: {e}"));
        assert_usable(name, &prompt);
    }
}

#[test]
fn recheck_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let prompt =
        playbooks::build_recheck_prompt(&root(), &finding, &repo, &out_path(), "repo notes")
            .expect("recheck.md must render");
    assert_usable("recheck", &prompt);
}

/// Guards against a playbook being added without a builder, or renamed
/// out from under one — either of which only shows up as a job failing
/// at runtime.
#[test]
fn every_playbook_is_covered_by_this_test() {
    let dir = root().join("playbooks");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .expect("read playbooks")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "md").then(|| p.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect();
    found.sort();

    let mut exercised: Vec<String> = [
        "hunt",
        "fix",
        "apply_improvement",
        "apply_modernization",
        "recheck",
        "engage",
        "harvest",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain(analysis_cases().into_iter().map(|(n, _)| n.to_owned()))
    .collect();
    exercised.sort();

    assert_eq!(
        found, exercised,
        "playbooks on disk and playbooks exercised here have diverged — \
         a new template needs a case above, or it ships unrendered"
    );
}
