#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The `standards` job's output contract, end to end.
//!
//! `hunter/playbooks/standards.md` tells the worker exactly which JSON keys
//! to emit; `ingest::type_required_fields` decides which are mandatory.
//! Nothing but this test makes the two agree, and disagreement is silent:
//! every standards finding would be counted invalid and dropped, with the
//! job still reporting success. Cheap test, whole feature riding on it.

use std::path::Path;

use hunter::domain::{FindingType, ForgeName};
use hunter::store::Store;

mod support;
use support::TempDir;

/// The keys the playbook actually instructs the worker to emit.
fn playbook_entry() -> serde_json::Value {
    serde_json::json!({
        "fingerprint": "widget:src/lib.rs:parse:type-safety",
        "type": "standards",
        "file": "src/lib.rs",
        "symbol": "parse",
        "line": 12,
        "severity": "medium",
        "confidence": 0.9,
        "summary": "raw String where a domain type is specified",
        "detail": "the standard requires domain types over primitives",
        "evidence_plan": "cargo clippy passes after the change",
        "standard_section": "Type safety / Domain types over primitives",
        "current_approach": "takes a String",
        "proposed_approach": "takes a RepoName"
    })
}

/// The scratch directory is handed back, and must stay bound by the caller:
/// it is what removes the database, and `let` bindings drop in reverse order
/// of appearance, so a named binding first in the tuple outlives the `Store`.
async fn ingest_one(
    entry: &serde_json::Value,
) -> (TempDir, Store, hunter::ingest::IngestResult, i64) {
    let dir = TempDir::new("standards-contract");
    let db = dir.join("hunter.db");
    let store = Store::connect(&db).await.expect("bootstrap db");
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
    let path = dir.join("findings.json");
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!([entry])).unwrap(),
    )
    .unwrap();
    let res = hunter::ingest::ingest_findings(
        &store,
        repo_id,
        Path::new(&path),
        Some(FindingType::Standards),
        None,
        None,
    )
    .await;
    (dir, store, res, repo_id)
}

/// A finding shaped exactly as the playbook specifies must be STORED.
#[tokio::test]
async fn a_playbook_shaped_standards_finding_is_accepted() {
    let (_dir, store, res, _repo) = ingest_one(&playbook_entry()).await;
    assert_eq!(
        res.invalid, 0,
        "playbook-shaped finding rejected as invalid"
    );
    assert_eq!(res.inserted, 1, "finding was not stored");

    let stored = store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap();
    let f = stored.first().expect("one finding");
    assert_eq!(
        f.standard_section.as_deref(),
        Some("Type safety / Domain types over primitives"),
        "the cited standard must be persisted, not dropped"
    );
    // Every other finding type exposes its class via category(); standards
    // was the only one returning None.
    assert_eq!(
        f.category(),
        Some(Some(
            "Type safety / Domain types over primitives".to_owned()
        ))
    );
}

/// Dropping the cited standard makes the finding invalid: the playbook says
/// every finding MUST cite a section, so this is the contract's teeth.
#[tokio::test]
async fn a_standards_finding_without_its_section_is_invalid() {
    let mut e = playbook_entry();
    e.as_object_mut().unwrap().remove("standard_section");
    let (_dir, _store, res, _repo) = ingest_one(&e).await;
    assert_eq!(res.inserted, 0);
    assert_eq!(
        res.invalid, 1,
        "a finding citing no standard must be rejected"
    );
}
