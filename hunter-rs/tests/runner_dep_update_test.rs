#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_dep_update` — what the scheduler does with Renovate's answer.
//!
//! `dep_scan::scan_repo` distinguishes "every dependency looked up is
//! current" (`Some(vec![])`) from "Renovate has no answer" (`None`: failed,
//! or looked nothing up). The runner has to keep that distinction: only
//! `None` may fall back to the model, because the fallback is a whole AI
//! run — and it must, or a repo Renovate cannot read is never analysed.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hunter::domain::{ForgeName, JobKind, JobState, RepoJobKind};
use hunter::scheduler::CycleSummary;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

/// One cycle of `run_dep_update` over a real clone, with the installed
/// Renovate replaced by one printing `renovate_output` and exiting 0.
/// Returns the summary, whether the model ran, and whether the repo's
/// dep-update cadence advanced.
async fn dep_update_cycle(label: &str, renovate_output: &str) -> (CycleSummary, bool, bool) {
    let c = cycle_with(label, renovate_output, None).await;
    (c.result.expect("dep_update cycle"), c.model_ran, c.advanced)
}

struct Cycle {
    result: anyhow::Result<CycleSummary>,
    model_ran: bool,
    advanced: bool,
    findings: i64,
}

/// [`dep_update_cycle`], optionally with a read-only result file an
/// earlier scan left behind (`stale`), so the new result cannot be written.
async fn cycle_with(label: &str, renovate_output: &str, stale: Option<&str>) -> Cycle {
    let bins = FakeBins::acquire(label);
    bins.ok("renovate", renovate_output);
    let dir = TempDir::new(&format!("{label}-root"));
    let origin_repo = GitRepo::with_branch(&dir, "unused");
    let origin = origin_repo.origin.to_string_lossy().into_owned();
    let (db, store) = fresh_store(&dir, "depupd").await;
    let repos_root = dir.subdir("repos");
    // GitLab-hosted, so no `gh` token lookup: the test is about the answer.
    let rid = store
        .add_repo(
            "widget",
            &origin,
            &repos_root,
            &origin_repo.default_branch,
            ForgeName::Gitlab,
        )
        .await
        .unwrap();
    let repo = store.get_repo_by_id(rid).await.unwrap().unwrap();
    // `run_dep_update` only works in an existing clone.
    git(dir.path(), &["clone", &origin, &repo.path]);
    // Hermetic stub for the fallback's prompt; the scripted worker ignores it.
    std::fs::write(
        dir.subdir("playbooks").join("dep_update.md"),
        "dep update\n",
    )
    .unwrap();
    let mut cfg = hunter::config::Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    let model_ran = Arc::new(AtomicBool::new(false));
    let backend = ScriptedBackend::new({
        let model_ran = Arc::clone(&model_ran);
        move |_| {
            model_ran.store(true, Ordering::SeqCst);
            done()
        }
    });

    if let Some(stale) = stale {
        use std::os::unix::fs::PermissionsExt;
        let old = cfg
            .work_root
            .join("out")
            .join(format!("dep_scan_{rid}.json"));
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, stale).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    let result = hunter::scheduler::run_dep_update(&store, &cfg, &repo, &backend, None).await;

    assert_eq!(bins.calls_to("renovate").len(), 1, "{:?}", bins.calls());
    let after = store.get_repo_by_id(rid).await.unwrap().unwrap();
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
            .await
            .unwrap();
    let (findings,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM findings WHERE repo_id = ?1")
        .bind(rid)
        .fetch_one(&pool)
        .await
        .unwrap();
    Cycle {
        result,
        model_ran: model_ran.load(Ordering::SeqCst),
        advanced: after.last_dep_update_at.is_some(),
        findings,
    }
}

#[tokio::test]
async fn up_to_date_repo_does_not_fall_back_to_the_model() {
    let current = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0","datasource":"npm","updates":[]}]}]}}"#;

    let (summary, model_ran, advanced) = dep_update_cycle("depupd-current", current).await;

    assert!(
        !model_ran,
        "renovate answered 'nothing to update', yet the model ran"
    );
    assert_eq!(summary.state, Some(JobState::Done));
    assert_eq!(
        summary.ingest.as_ref().map(|i| (i.inserted, i.invalid)),
        Some((0, 0)),
        "{summary:?}"
    );
    assert!(
        advanced,
        "the cadence must advance, or the repo is rescanned every cycle"
    );
}

/// What Renovate 44.96.2 prints for a repo it finds no dependency in.
#[tokio::test]
async fn repo_renovate_cannot_read_falls_back_to_the_model() {
    let no_deps = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{}}"#;

    let (summary, model_ran, _) = dep_update_cycle("depupd-nodeps", no_deps).await;

    assert!(model_ran, "renovate had no answer, so the model must run");
    assert!(summary.job_id.is_some(), "{summary:?}");
}

/// Renovate's updates become `dep_update` findings, and the cycle summary
/// (the daemon's per-cycle log line and the manual-trigger response) says
/// which job ran for which repo and what it ingested.
#[tokio::test]
async fn updates_are_ingested_and_reported() {
    let update = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"^1.2.0","datasource":"npm","updates":[{"newVersion":"1.3.0","updateType":"minor"}]}]}]}}"#;

    let (summary, model_ran, advanced) = dep_update_cycle("depupd-update", update).await;

    assert!(!model_ran, "renovate answered, so no model run");
    assert_eq!(summary.kind, Some(JobKind::Repo(RepoJobKind::DepUpdate)));
    assert_eq!(summary.repo.as_deref(), Some("widget"));
    assert_eq!(
        summary
            .ingest
            .as_ref()
            .map(|i| (i.inserted, i.duplicates, i.invalid)),
        Some((1, 0, 0)),
        "{summary:?}"
    );
    assert!(advanced);
}

/// When the result file cannot be written, the cycle fails instead of
/// ingesting whatever an earlier scan left at that path: here a stale
/// candidate would be re-filed and the cadence advanced, for a repo whose
/// scan just said everything is current.
#[tokio::test]
async fn unwritable_result_file_fails_the_cycle() {
    let current = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0","datasource":"npm","updates":[]}]}]}}"#;
    let stale = r#"[{"fingerprint":"widget:npm:left-pad:1.2.0→left-pad 1.3.0","file":"package.json","ecosystem":"npm","package":"left-pad","current_version":"1.2.0","latest_version":"1.3.0","update_type":"minor","severity":"medium","confidence":0.85,"summary":"left-pad: minor update","detail":"stale"}]"#;

    let c = cycle_with("depupd-stale", current, Some(stale)).await;

    assert!(c.result.is_err(), "{:?}", c.result);
    assert_eq!(c.findings, 0, "the stale result was ingested");
    assert!(!c.advanced, "a failed cycle must not advance the cadence");
    assert!(!c.model_ran);
}
