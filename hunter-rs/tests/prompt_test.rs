#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! What the prompt builders tell a worker, rendered from stub templates.
//!
//! `playbook_contract_test` renders the real playbooks, which proves each
//! builder supplies every slot its template names, but not what it puts
//! in them. That is checked here, and stubs are the right tool for it: a
//! template made of nothing but slots renders exactly what the builder
//! supplied.
//!
//! The value that matters most is where the worker works. A job runs in
//! its chain's own tree (`jobs/<origin>/tree`); the clone the repo row
//! names is shared by every job on the repo, so a prompt that sent the
//! worker there would have it read, and edit, a checkout that is not the
//! one its chain pinned.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::FindingType;
use hunter::playbooks;
use hunter::types::{Finding, Repo};
use support::{TempDir, sample_finding, sample_repo};

/// Shared shape of the five repo-level analysis prompt builders.
type AnalysisBuilder = fn(
    &Path,
    &Repo,
    &Path,
    &str,
    &[Finding],
    &[Finding],
    &Path,
    i64,
    &str,
) -> anyhow::Result<String>;

/// One slot per line, named as the builder names it.
const COMMON: &str = "tree={{REPO_PATH}}\nout={{OUT_PATH}}\nrepo={{REPO_NAME}}\n";

/// A project root holding a stub of every template under test, plus the
/// `CODING_STANDARDS.md` the standards builder reads beside it.
fn stub_root(dir: &TempDir) -> PathBuf {
    let root = dir.subdir("hunter");
    let playbooks = dir.subdir("hunter/playbooks");
    for (name, max_slot) in [
        ("test_gap.md", "MAX_GAPS"),
        ("dep_update.md", "MAX_UPDATES"),
        ("refactor.md", "MAX_REFACTORS"),
        ("modernization.md", "MAX_MODERNIZATIONS"),
    ] {
        std::fs::write(
            playbooks.join(name),
            format!("{COMMON}max={{{{{max_slot}}}}}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        playbooks.join("standards.md"),
        format!("{COMMON}max={{{{MAX_FINDINGS}}}}\nstandards={{{{STANDARDS}}}}\n"),
    )
    .unwrap();
    std::fs::write(
        playbooks.join("recheck.md"),
        format!("{COMMON}finding={{{{FINDING_JSON}}}}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("CODING_STANDARDS.md"),
        "Name things for what they mean.\n",
    )
    .unwrap();
    root
}

/// The value rendered into `slot`, from a `slot=value` line.
fn slot<'a>(prompt: &'a str, slot: &str) -> Option<&'a str> {
    prompt
        .lines()
        .find_map(|l| l.strip_prefix(slot)?.strip_prefix('='))
}

/// A chain's tree and output path, deliberately nowhere near the clone.
fn chain_paths(dir: &TempDir) -> (PathBuf, PathBuf) {
    let chain = dir.join("work_root/jobs/7");
    (chain.join("tree"), chain.join("session/out.json"))
}

#[test]
fn analysis_prompts_send_the_worker_to_the_chains_tree() {
    let dir = TempDir::new("prompt-analysis");
    let root = stub_root(&dir);
    let repo = sample_repo();
    let (tree, out) = chain_paths(&dir);

    for (name, build) in [
        (
            "test_gap",
            playbooks::build_test_gap_prompt as AnalysisBuilder,
        ),
        ("dep_update", playbooks::build_dep_update_prompt),
        ("refactor", playbooks::build_refactor_prompt),
        ("modernization", playbooks::build_modernization_prompt),
        ("standards", playbooks::build_standards_prompt),
    ] {
        let prompt = build(&root, &repo, &tree, "scope", &[], &[], &out, 5, "notes").unwrap();

        assert_eq!(slot(&prompt, "tree"), tree.to_str(), "{name}: {prompt}");
        assert_eq!(slot(&prompt, "out"), out.to_str(), "{name}: {prompt}");
        assert_eq!(slot(&prompt, "repo"), Some("widget"), "{name}: {prompt}");
        assert_eq!(slot(&prompt, "max"), Some("5"), "{name}: {prompt}");
        assert!(
            !prompt.contains(&repo.path),
            "{name} names the shared clone: {prompt}"
        );
    }
}

/// The standards audit is pointless without the document it audits
/// against, so the builder carries it into the prompt itself.
#[test]
fn the_standards_prompt_carries_the_standards_document() {
    let dir = TempDir::new("prompt-standards");
    let root = stub_root(&dir);
    let (tree, out) = chain_paths(&dir);

    let prompt = playbooks::build_standards_prompt(
        &root,
        &sample_repo(),
        &tree,
        "scope",
        &[],
        &[],
        &out,
        5,
        "notes",
    )
    .unwrap();

    assert_eq!(
        slot(&prompt, "standards"),
        Some("Name things for what they mean.")
    );
}

#[test]
fn the_recheck_prompt_sends_the_worker_to_the_chains_tree() {
    let dir = TempDir::new("prompt-recheck");
    let root = stub_root(&dir);
    let repo = sample_repo();
    let (tree, out) = chain_paths(&dir);
    let finding = sample_finding(FindingType::Bug);

    let prompt =
        playbooks::build_recheck_prompt(&root, &finding, &repo, &tree, &out, "notes").unwrap();

    assert_eq!(slot(&prompt, "tree"), tree.to_str(), "{prompt}");
    assert_eq!(slot(&prompt, "out"), out.to_str(), "{prompt}");
    assert!(
        prompt.contains(&finding.fingerprint),
        "the finding to recheck: {prompt}"
    );
    assert!(
        !prompt.contains(&repo.path),
        "names the shared clone: {prompt}"
    );
}
