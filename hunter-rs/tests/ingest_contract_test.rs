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

use hunter::domain::{FindingJobKind, FindingType, ForgeName, JobState, RepoJobKind, Severity};
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

/// Two bug entries, ingested as `job`. Returns how many were inserted; the
/// ids come from `produced_by`.
async fn hunt_producing_two(dir: &TempDir, store: &Store, repo_id: i64, job: i64) -> i64 {
    let entries = serde_json::json!([
        {
            "fingerprint": "widget:src/a.rs:f:1", "type": "bug", "file": "src/a.rs",
            "bug_class": "boundary", "severity": "high", "confidence": 0.9, "summary": "first"
        },
        {
            "fingerprint": "widget:src/b.rs:g:2", "type": "bug", "file": "src/b.rs",
            "bug_class": "boundary", "severity": "low", "confidence": 0.8, "summary": "second"
        }
    ]);
    let path = dir.join("findings.json");
    std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
    hunter::ingest::ingest_findings(
        store,
        repo_id,
        Path::new(&path),
        Some(FindingType::Bug),
        Some(job),
        None,
    )
    .await
    .inserted
}

async fn provenance_fixture() -> (TempDir, Store, i64) {
    let dir = TempDir::new("ingest-provenance");
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
    (dir, store, repo_id)
}

async fn produced_by(store: &Store, job: i64) -> Vec<i64> {
    let mut ids = store
        .list_jobs(50)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.job.id == job)
        .expect("job is listed")
        .produced_finding_ids;
    ids.sort_unstable();
    ids
}

/// A hunt's findings must be reachable from the job that produced them.
///
/// Nothing recorded this before: ingest had the job id and passed it to
/// every event except the one marking the finding's creation, so the
/// only trace of "this hunt found that bug" was a `+N new` counter in a
/// log message.
#[tokio::test]
async fn findings_are_attributed_to_the_hunt_that_produced_them() {
    let (dir, store, repo_id) = provenance_fixture().await;
    let job = store
        .create_job(
            RepoJobKind::Hunt.into(),
            repo_id,
            None,
            1000,
            JobState::Running,
            None,
        )
        .await
        .unwrap();

    let inserted = hunt_producing_two(&dir, &store, repo_id, job).await;
    assert_eq!(inserted, 2, "fixture: both entries are new");
    assert_eq!(
        produced_by(&store, job).await.len(),
        2,
        "both findings must point back at the hunt that produced them"
    );
}

/// A rediscovery belongs to the job that first turned it up.
///
/// Hunts re-run over the same code constantly, so without this the
/// attribution would drift to whichever hunt last saw the finding, and
/// "which hunt found this?" would answer "the most recent one" forever.
#[tokio::test]
async fn a_rediscovery_does_not_steal_attribution() {
    let (dir, store, repo_id) = provenance_fixture().await;
    let first = store
        .create_job(
            RepoJobKind::Hunt.into(),
            repo_id,
            None,
            1000,
            JobState::Running,
            None,
        )
        .await
        .unwrap();
    assert_eq!(hunt_producing_two(&dir, &store, repo_id, first).await, 2);
    let original = produced_by(&store, first).await;

    let second = store
        .create_job(
            RepoJobKind::Hunt.into(),
            repo_id,
            None,
            1000,
            JobState::Running,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        hunt_producing_two(&dir, &store, repo_id, second).await,
        0,
        "fixture: the same two entries are duplicates now"
    );

    assert_eq!(
        produced_by(&store, first).await,
        original,
        "the original finder keeps its findings"
    );
    assert!(
        produced_by(&store, second).await.is_empty(),
        "a hunt that only rediscovered known findings produced nothing"
    );
}

/// A job handed a finding produces none: `finding_id` and
/// `produced_finding_ids` are opposite directions and never both set.
#[tokio::test]
async fn a_job_given_a_finding_produces_nothing() {
    let (dir, store, repo_id) = provenance_fixture().await;
    let hunt = store
        .create_job(
            RepoJobKind::Hunt.into(),
            repo_id,
            None,
            1000,
            JobState::Running,
            None,
        )
        .await
        .unwrap();
    hunt_producing_two(&dir, &store, repo_id, hunt).await;
    let found = produced_by(&store, hunt).await;

    let fix = store
        .create_job(
            FindingJobKind::Fix.into(),
            repo_id,
            Some(found[0]),
            1000,
            JobState::Running,
            None,
        )
        .await
        .unwrap();
    let entry = store
        .list_jobs(50)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.job.id == fix)
        .unwrap();
    assert!(entry.produced_finding_ids.is_empty());
    assert_eq!(entry.job.finding_id, Some(found[0]));
}

/// A confidence that parses but is not a number a worker meant is refused
/// at validation, with a reason that says so.
///
/// `"NaN"` and `"inf"` both parse as `f64`. NaN survives the clamp and binds
/// as NULL into `confidence REAL NOT NULL`, so the old path also ended in
/// `invalid == 1` -- via a database error whose message named nothing
/// useful, which is why this asserts the logged reason and not just the
/// count. Infinity clamped to 1.0 and was stored as a full-confidence
/// finding.
#[tokio::test]
async fn a_non_finite_confidence_is_refused_by_validation() {
    for raw in ["NaN", "inf", "-inf"] {
        let dir = TempDir::new("ingest-nonfinite");
        let store = Store::connect(&dir.join("hunter.db"))
            .await
            .expect("bootstrap");
        let repo_id = store
            .add_repo(
                "widget",
                "git@github.com:acme/widget.git",
                Path::new("/tmp/wr/repos"),
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
            "severity": "high",
            "confidence": raw,
            "summary": "off-by-one in the parser",
            "detail": "…",
            "evidence_plan": "failing test first"
        }]);
        let path = dir.join("findings.json");
        std::fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
        let res = hunter::ingest::ingest_findings(
            &store,
            repo_id,
            &path,
            Some(FindingType::Bug),
            None,
            None,
        )
        .await;

        assert_eq!(res.inserted, 0, "{raw}: nothing may be stored");
        assert_eq!(res.invalid, 1, "{raw}: the entry is invalid");
        let events = store.recent_events(10).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("non-finite confidence")),
            "{raw}: the refusal must name the reason, got {:?}",
            events.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }
}
