#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! What ingest writes has to be readable back.
//!
//! Validation and persistence disagree easily here: the validators accept
//! what a worker plausibly emits, while the columns are decoded into the
//! domain enums, which accept only their own canonical spelling. A field
//! that passes validation and is then stored verbatim produces a row that
//! cannot be read — and because the reads are whole-list queries, one such
//! row fails every later query for that repo, not just its own.

use std::path::Path;

use hunter::domain::{FindingType, ForgeName, Severity};
use hunter::store::{FindingFilter, Store};

mod support;
use support::TempDir;

/// The scratch directory is handed back, and must stay bound by the caller:
/// it is what removes the database, and `let` bindings drop in reverse order
/// of appearance, so a named binding first in the tuple outlives the `Store`.
async fn ingest_severity(raw: &str) -> (TempDir, Store, hunter::ingest::IngestResult) {
    let dir = TempDir::new("ingest-contract");
    let store = Store::connect(&dir.join("hunter.db"))
        .await
        .expect("bootstrap");
    let repo_id = store
        .add_repo(
            "widget",
            "git@github.com:acme/widget.git",
            std::path::Path::new("/tmp/wr/repos"),
            "main",
            ForgeName::Github,
        )
        .await
        .unwrap();
    let entry = serde_json::json!([{
        "fingerprint": format!("widget:src/lib.rs:parse:{raw}"),
        "type": "bug",
        "file": "src/lib.rs",
        "line": 12,
        "bug_class": "boundary",
        "severity": raw,
        "confidence": 0.9,
        "summary": "off-by-one in the parser",
        "detail": "…",
        "evidence_plan": "failing test first"
    }]);
    let path = dir.join("findings.json");
    std::fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
    let res = hunter::ingest::ingest_findings(
        &store,
        repo_id,
        Path::new(&path),
        Some(FindingType::Bug),
        None,
        None,
    )
    .await;
    (dir, store, res)
}

/// `Severity::parse` is case-insensitive, so `"HIGH"` is *accepted*. It must
/// therefore also be stored canonically: the column is read back as a
/// `Severity`, which only decodes lowercase.
#[tokio::test]
async fn an_uppercase_severity_is_stored_canonically_and_reads_back() {
    let (_dir, store, res) = ingest_severity("HIGH").await;
    assert_eq!(
        res.invalid, 0,
        "uppercase severity is accepted by validation"
    );
    assert_eq!(res.inserted, 1);

    // The read is the point: it decodes the column into `Severity`.
    let all = store
        .list_findings(&FindingFilter::default())
        .await
        .expect("a stored finding must be readable");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].severity, Severity::High);
}

/// The whole-list read is what makes one bad row expensive: it is not the
/// offending finding that fails, it is every query that touches it.
#[tokio::test]
async fn one_badly_stored_finding_does_not_break_reads_for_the_repo() {
    let (_dir, store, _) = ingest_severity("High").await;
    let known = store
        .known_active(1, FindingType::Bug.as_str())
        .await
        .expect("novelty comparison must not fail on a stored row");
    assert_eq!(known.len(), 1);
    assert_eq!(known[0].severity, Severity::High);
}

/// `bug_class` is only meaningful — and only validated — for bug entries.
///
/// A non-bug entry carrying one was stored verbatim, so a `refactor`
/// finding with `"bug_class": "Logic"` wrote a value `BugClass` cannot
/// decode. Same blast radius as the severity case: the reads are
/// whole-list queries, so the bad row takes every later read with it.
#[tokio::test]
async fn a_non_bug_entry_cannot_smuggle_in_a_bug_class() {
    let dir = TempDir::new("ingest-bugclass");
    let store = Store::connect(&dir.join("hunter.db"))
        .await
        .expect("bootstrap");
    let repo_id = store
        .add_repo(
            "widget",
            "git@github.com:acme/widget.git",
            std::path::Path::new("/tmp/wr/repos"),
            "main",
            ForgeName::Github,
        )
        .await
        .unwrap();
    let entry = serde_json::json!([{
        "fingerprint": "widget:src/lib.rs:parse:dup",
        "type": "refactor",
        "file": "src/lib.rs",
        "severity": "low",
        "confidence": 0.8,
        "summary": "duplicated parsing branches",
        // Not a valid BugClass, and not validated for this type.
        "bug_class": "Logic",
        "smell_type": "duplication",
        "suggested_refactor": "extract a helper"
    }]);
    let path = dir.join("findings.json");
    std::fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
    let res =
        hunter::ingest::ingest_findings(&store, repo_id, Path::new(&path), None, None, None).await;
    assert_eq!(res.inserted, 1, "a valid refactor entry is still accepted");

    let all = store
        .list_findings(&FindingFilter::default())
        .await
        .expect("the stored row must be readable");
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].bug_class, None,
        "a refactor finding has no bug class"
    );
}
