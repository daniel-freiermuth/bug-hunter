#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Rotation fairness: what an unsuccessful analysis attempt does to
//! `last_<kind>_at`.
//!
//! `run_cycle_inner` bumps that timestamp when an analysis-kind attempt
//! did not succeed, so a kind that fails forever stops being the
//! perpetual "oldest" pick and its siblings get a turn. The condition
//! that decides "did not succeed" is one boolean expression, and the two
//! cases below are its edges: a kill is a failed turn and must bump, a
//! suspension is a pause and must not.
//!
//! Driven through `run_cycle` rather than the executor, because the bump
//! lives in the cycle and not in `run_analysis_job` — calling the
//! executor directly would assert on nothing at all.

mod support;

use std::path::PathBuf;

use hunter::config::Config;
use hunter::domain::{JobState, RepoJobKind};
use hunter::scheduler::{Candidate, pick_next, run_cycle};
use hunter::store::Store;
use hunter::types::RunResult;
use sqlx::SqlitePool;
use support::{GitRepo, ScriptedBackend, TempDir};

/// Where `test_gap`'s rotation timestamp starts: long ago, and the
/// stalest of the eligible kinds, so rotation picks it.
const STALE_TEST_GAP: i64 = 1_000;
/// `dep_update`'s: stale enough to be eligible, newer than `test_gap`,
/// so it is the kind that gets the turn once `test_gap` has had one.
const STALE_DEP_UPDATE: i64 = 3_000;
const STALE_REFACTOR: i64 = 4_000;
const STALE_HUNT: i64 = 5_000;

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the seed pool and the `Store` opened on `path`, and SQLite's handles
/// are closed before the files go.
async fn fixture(label: &str) -> (TempDir, PathBuf, SqlitePool, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "rotation").await;

    // A real clone with a real origin: `run_analysis_job` syncs before it
    // spawns anything, and refuses a directory whose origin is not the
    // repo's URL.
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.path().join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();

    // modernization and standards are parked in the future so their much
    // longer intervals cannot make them eligible; hunt/dep_update/
    // refactor are eligible but newer than test_gap.
    let future = hunter::util::now_ms() + 999_999;
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?5, ?6, ?7, ?7)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(STALE_HUNT)
    .bind(STALE_TEST_GAP)
    .bind(STALE_DEP_UPDATE)
    .bind(STALE_REFACTOR)
    .bind(future)
    .execute(&pool)
    .await
    .unwrap();

    // Hermetic stub: the scripted worker ignores the prompt, so all the
    // template has to do is render.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("test_gap.md"),
        "test gaps in {{REPO_NAME}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");

    (dir, path, pool, cfg)
}

/// A worker that writes no output and stops for `reason`, leaving
/// `session` behind (or not).
fn stopped_worker(reason: &'static str, session: Option<PathBuf>) -> ScriptedBackend {
    ScriptedBackend::new(move |_| RunResult {
        exit_code: None,
        killed_reason: Some(reason.to_owned()),
        tokens_new: 30_000,
        calls: 4,
        session_file: session.as_ref().map(|p| p.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    })
}

async fn last_test_gap_at(store: &Store) -> i64 {
    store
        .get_repo_by_id(1)
        .await
        .unwrap()
        .expect("repo 1")
        .last_test_gap_at
        .expect("seeded non-NULL")
}

/// A killed analysis attempt bumps its rotation timestamp.
///
/// The turn was spent: something ran, and it is over. Leaving the
/// timestamp at its old value would make this kind the stalest pick
/// again next cycle, and the cycle after that — a kind that fails
/// reliably would hold the rotation slot forever while its siblings
/// never ran again.
#[tokio::test]
async fn a_killed_analysis_attempt_bumps_the_rotation_timestamp() {
    let (dir, path, _pool, cfg) = fixture("rotation-killed").await;
    let store = Store::connect(&path).await.unwrap();
    // A wallclock kill is never a suspension, transcript or not, so this
    // is the plain killed outcome.
    let backend = stopped_worker("wallclock", Some(dir.path().join("session.jsonl")));

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(
        summary.kind.map(|k| k.to_string()),
        Some("test_gap".to_owned())
    );
    assert_eq!(summary.state, Some(JobState::Killed), "{summary:?}");
    assert!(
        last_test_gap_at(&store).await > STALE_TEST_GAP,
        "a killed attempt must still bump last_test_gap_at, or this kind \
         keeps winning the oldest-first rotation forever"
    );

    // And the turn really did move on: nothing but the timestamp decides
    // who is picked next.
    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::DepUpdate,
                ..
            })
        ),
        "after test_gap's turn the next-stalest kind must be selected, got {picked:?}"
    );
}

/// A suspended analysis attempt does NOT bump its rotation timestamp.
///
/// A cap kill with a transcript is a pause, and re-selecting that work
/// belongs to the resume tier, which claims the job by id at a priority
/// above rotation. Bumping here would record a turn as taken while the
/// work had not started — and the job would then be offered by the
/// resume tier and skipped by rotation at the same time.
#[tokio::test]
async fn a_suspended_analysis_attempt_does_not_bump_the_rotation_timestamp() {
    let (dir, path, _pool, cfg) = fixture("rotation-suspended").await;
    let store = Store::connect(&path).await.unwrap();
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, "{}\n").unwrap();
    // A cap kill that left a transcript is what the harness reports when
    // the window ran out of headroom mid-run.
    let backend = stopped_worker("cap", Some(session));

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(
        summary.kind.map(|k| k.to_string()),
        Some("test_gap".to_owned())
    );
    assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
    assert_eq!(
        last_test_gap_at(&store).await,
        STALE_TEST_GAP,
        "a suspension must leave last_test_gap_at alone: the scan was paused \
         before it produced anything, so bumping it records a turn that was \
         never taken. The resume tier owns re-selecting this work, so a bump \
         would have the same job offered as a resume AND passed over by \
         rotation, and the repo's record would claim a scan ran when none did"
    );

    // The other half of the same contract: the work is not lost by being
    // left out of the bump, it is claimed one tier higher.
    let picked = pick_next(&store, &cfg, None).await.unwrap();
    let plan = match picked {
        Some(Candidate::Resume { plan, .. }) => plan,
        other => panic!("the resume tier must claim the suspension, got {other:?}"),
    };
    assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
    assert_eq!(plan.kind.to_string(), "test_gap");
}
