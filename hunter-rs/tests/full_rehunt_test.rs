#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Full re-hunts: a hunt that reviews the repo's complete history
//! (`<empty tree>..<tip>`) instead of the commits since its watermark,
//! driven through the scheduler against a real clone.

mod support;

use std::path::{Path, PathBuf};

use hunter::config::Config;
use hunter::domain::JobState;
use hunter::scheduler::{Candidate, pick_next, run_hunt};
use hunter::store::Store;
use hunter::types::{Repo, RunResult};
use hunter::util::now_ms;
use support::{GitRepo, ScriptedBackend, TempDir, done, git};

/// Git's empty tree: the base of a diff range covering every commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

const USAGE: &str = r#"{"message":{"role":"assistant","usage":{"input":1000,"output":100,"cacheRead":0,"cacheWrite":500}}}"#;

struct Fixture {
    cfg: Config,
    store: Store,
    /// The tip of origin/main, which the repo's watermark already covers.
    head: String,
    /// Last, so the directory outlives the SQLite handles above.
    _dir: TempDir,
}

/// Repo 1, cloned and hunted up to its tip, with `last_full_hunt_at` set
/// to `last_full_hunt_at`.
async fn fixture(label: &str, last_full_hunt_at: i64) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.subdir("repos").join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    let head = git(&clone, &["rev-parse", "origin/main"]).trim().to_owned();
    let (path, pool) = support::fresh_pool(&dir, "rehunt").await;
    let now = now_ms();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at, \
         last_hunt_sha, last_hunt_at, last_full_hunt_at, last_test_gap_at, last_dep_update_at, \
         last_refactor_at, last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?5, ?4, ?4, ?4, ?4, ?4)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(&head)
    .bind(now)
    .bind(last_full_hunt_at)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let store = Store::connect(&path).await.unwrap();
    std::fs::write(
        dir.subdir("playbooks").join("hunt.md"),
        "hunt {{REPO_PATH}} {{DIFF_RANGE}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        store,
        head,
        _dir: dir,
    }
}

impl Fixture {
    async fn repo(&self) -> Repo {
        self.store.get_repo_by_id(1).await.unwrap().unwrap()
    }

    /// A worker that finishes cleanly with no findings, writing the
    /// (empty) findings file its chain's playbook named.
    fn clean_worker(&self) -> ScriptedBackend {
        let out_dir = self.cfg.work_root.join("out");
        ScriptedBackend::new(move |tree| {
            std::fs::write(findings_path(&out_dir, tree), "[]").unwrap();
            done()
        })
    }
}

/// `<work_root>/out/job<origin>.findings.json` for the chain whose tree is
/// `<work_root>/jobs/<origin>/tree`.
fn findings_path(out_dir: &Path, tree: &Path) -> PathBuf {
    let origin = tree
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    out_dir.join(format!("job{origin}.findings.json"))
}

/// A worker stopped at its cap, transcript in the chain's session dir.
fn suspended_at_cap(tree: &Path) -> RunResult {
    let file = tree.parent().unwrap().join("session").join("session.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!("{USAGE}\n")).unwrap();
    RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(file.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

/// A periodic full re-hunt that suspends and is resumed is still a full
/// re-hunt when it finishes: it records `last_full_hunt_at`, so the next
/// cold hunt goes back to incremental instead of starting the whole
/// history over.
#[tokio::test]
async fn a_resumed_periodic_rehunt_records_its_full_hunt() {
    // Long past the 90-day re-hunt interval.
    let f = fixture("rehunt-resumed", 1_000).await;
    let row = f.repo().await;

    let cold = ScriptedBackend::new(suspended_at_cap);
    let first = run_hunt(&f.store, &f.cfg, &row, &cold, None).await.unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    assert_eq!(first.full_rehunt, Some(true));

    let plan = match pick_next(&f.store, &f.cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended hunt to be resumed, got {other:?}"),
    };
    let warm = f.clean_worker();
    let resumed = run_hunt(&f.store, &f.cfg, &f.repo().await, &warm, Some(&plan))
        .await
        .unwrap();
    assert_eq!(resumed.state, Some(JobState::Done), "{resumed:?}");
    assert_eq!(
        resumed.full_rehunt,
        Some(true),
        "the resume kept full scope"
    );
    assert_eq!(
        resumed.diff_range.as_deref(),
        Some(format!("{EMPTY_TREE}..{}", f.head).as_str())
    );

    let after = f.repo().await;
    assert!(
        after.last_full_hunt_at.unwrap() > 1_000,
        "a resumed full re-hunt records its full hunt"
    );
    assert_eq!(after.last_hunt_sha.as_deref(), Some(f.head.as_str()));

    // The next cold hunt is incremental again: nothing new, so skipped.
    let next = run_hunt(&f.store, &f.cfg, &after, &f.clean_worker(), None)
        .await
        .unwrap();
    assert_eq!(next.skipped.as_deref(), Some("no new commits"), "{next:?}");
}
