#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_fix` — worktree setup.
//!
//! The branch a fix job works on is named after the finding, so it is the
//! same name on every attempt. Any attempt that ends without deleting the
//! branch (the reclaim path only fires when the worktree *directory* is
//! still there) leaves `git worktree add -b` failing with "a branch named
//! ... already exists" — on that cycle and on every cycle after it, since
//! nothing about waiting changes it. Observed 2026-09-15: five
//! consecutive cycles two minutes apart, all for finding 3304, ended only
//! by restarting the daemon.

mod support;

use hunter::config::Config;
use hunter::domain::ForgeName;
use hunter::store::FindingInsert;
use support::{GitRepo, ScriptedBackend, TempDir, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";
/// Matches the slug `run_fix` derives from the finding summary below.
const BRANCH: &str = "fix/a-real-bug-1";

struct Fixture {
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    repo_dir: std::path::PathBuf,
    /// Last, so the directory outlives the Store's SQLite pool. See
    /// `runner_engage_test` for why, and for what it does not fix.
    _dir: TempDir,
}

/// A repo with a queued bug finding — the state `run_fix` expects.
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "some-other-branch");
    let (_db, store) = fresh_store(&dir, "fix").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            REPO_URL,
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    let repo_dir = hunter::store::Store::repo_dir(&repos_root, rid);
    std::fs::rename(&repo.work, &repo_dir).unwrap();

    let (fid, _) = store
        .upsert_finding(
            rid,
            &FindingInsert {
                fingerprint: "fp-fix-1".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: "a real bug".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store
        .set_finding_status(fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();

    // Hermetic stub: the scripted worker ignores the prompt, so all the
    // template has to do is render.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("fix.md"), "fix {{WORKTREE}}\n").unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        _dir: dir,
        cfg,
        store,
        fid,
        repo_dir,
    }
}

/// The regression: a leftover branch from an abandoned attempt, with no
/// worktree registered for it, must not wedge every later attempt.
#[tokio::test]
async fn leftover_branch_without_a_worktree_does_not_wedge_the_fix() {
    let f = fixture("fix-leftover-branch").await;
    git(&f.repo_dir, &["branch", BRANCH, "main"]);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // Declining needs no forge: the run ends at NOT-A-BUG.md, so what is
    // under test is the worktree setup that precedes it.
    let backend = ScriptedBackend::writing("NOT-A-BUG.md", "misread the code");
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .expect("a leftover branch is reclaimable, not a permanent failure");

    assert_eq!(summary.branch.as_deref(), Some(BRANCH));
    assert_eq!(
        summary.outcome.as_deref(),
        Some("rejected"),
        "the worker ran and its verdict was recorded: {summary:?}"
    );
}

/// The same branch, still checked out by a registration whose tree is
/// gone and which git left locked: what a daemon killed during
/// `git worktree add` leaves behind. `prune` skips a locked worktree and
/// `branch -D` refuses a checked-out branch, so without unlocking it every
/// later attempt failed (observed 2026-09-29: 400 cycles, finding 3949).
#[tokio::test]
async fn leftover_branch_held_by_a_killed_worktree_add_does_not_wedge_the_fix() {
    let f = fixture("fix-locked-leftover").await;
    let tree = f.cfg.work_root.join("jobs").join("999").join("tree");
    let t = tree.to_string_lossy();
    git(&f.repo_dir, &["worktree", "add", "-b", BRANCH, &t, "main"]);
    git(
        &f.repo_dir,
        &["worktree", "lock", "--reason", "initializing", &t],
    );
    std::fs::remove_dir_all(tree.parent().unwrap()).unwrap();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("NOT-A-BUG.md", "misread the code");
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(
        summary.outcome.as_deref(),
        Some("rejected"),
        "the worker ran and its verdict was recorded: {summary:?}"
    );
}

/// The same run with no leftover branch, so the test above is known to be
/// asserting on a difference rather than on a path that always works.
#[tokio::test]
async fn clean_repo_runs_the_fix() {
    let f = fixture("fix-clean").await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("NOT-A-BUG.md", "misread the code");
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("rejected"));
}

// ---------------------------------------------------------------------------
// The cap the job is granted
// ---------------------------------------------------------------------------

/// The granted cap is the backend's headroom, unaltered.
///
/// It used to be `min(config cap, headroom)`, the config number being a
/// hand-picked constant (`fix.capNewTokens`, default 150 000). Such a
/// constant drifts below what the jobs it governs actually cost and then
/// kills work the ramp had already found room for, while the ramp's own
/// headroom — computed from live window state — bounds the same spend
/// correctly. Two bounds, one of them blind.
#[tokio::test]
async fn granted_cap_is_the_backend_headroom_verbatim() {
    let f = fixture("fix-cap-verbatim").await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // Deliberately above every cap the config used to impose, so a
    // surviving min() shows up as the config number instead.
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "misread the code").granting(Some(777_000));
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let jobs = f.store.list_jobs(10).await.unwrap();
    assert_eq!(
        jobs[0].job.cap_tokens,
        Some(777_000),
        "the job must run under the headroom the backend granted"
    );
}

/// A backend that grants no ceiling leaves the job with no token bound:
/// NULL in `jobs.cap_tokens`, `maxWallS` the only remaining stop
/// condition. The old fallback turned "the ramp sees no reason to bound
/// this" into "bound it at the config cap".
#[tokio::test]
async fn backend_without_a_ceiling_leaves_the_job_unbounded() {
    let f = fixture("fix-cap-none").await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("NOT-A-BUG.md", "misread the code").granting(None);
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let jobs = f.store.list_jobs(10).await.unwrap();
    assert_eq!(
        jobs[0].job.cap_tokens, None,
        "no headroom bound means no token bound, not the config's"
    );
}

/// A fix whose tree cannot be made goes back to `queued`, not stranded
/// in `fixing` until the next daemon restart.
#[tokio::test]
async fn fix_whose_tree_cannot_be_made_returns_the_finding_to_queued() {
    let f = fixture("fix-no-tree").await;
    // The first job's workspace is already taken, so creating it is refused.
    let taken = f.cfg.work_root.join("jobs").join("1").join("session");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("other.jsonl"), "someone else's\n").unwrap();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &never, None)
        .await
        .unwrap();
    assert_eq!(summary.state, Some(hunter::domain::JobState::Failed));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
}

/// The shipping path: a finished fix is pushed and its draft PR recorded,
/// both for a new PR and for the "already exists" recovery.
#[tokio::test]
async fn a_finished_fix_ships_and_records_its_draft_pr() {
    const URL: &str = "https://github.com/acme/widget/pull/42";
    for case in ["created", "exists"] {
        let bins = support::FakeBins::acquire("fix-ship");
        let f = fixture("fix-ship").await;
        // Push to the fixture's bare origin instead of the forge.
        let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
        git(
            &f.repo_dir,
            &[
                "config",
                &format!("url.{}.insteadOf", origin.trim()),
                "git@github.com:acme/widget.git",
            ],
        );
        if case == "created" {
            bins.ok("gh", URL);
        } else {
            bins.fail("gh", 1, &format!("a pull request already exists: {URL}"));
        }
        let worker = ScriptedBackend::new(|tree| {
            std::fs::write(tree.join("fix.txt"), "fixed\n").unwrap();
            git(tree, &["add", "fix.txt"]);
            git(tree, &["commit", "-m", "fix it"]);
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the fix").unwrap();
            support::done()
        });
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
            .await
            .unwrap();

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{case}");
        assert_eq!(summary.pr_url.as_deref(), Some(URL), "{case}");
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(finding.status.as_str(), "pr_open", "{case}");
        assert_eq!(finding.pr_url.as_deref(), Some(URL), "{case}");
        drop(bins);
    }
}

/// A verdict that lands after `pick_next` read the finding wins: the stale
/// fix creates no job, so it neither overwrites the operator's rejection
/// with `fixing` nor continues the checkpoint, which stays resumable if the
/// operator changes their mind.
#[tokio::test]
async fn verdict_after_selection_stops_the_fix_before_its_job() {
    let f = fixture("fix-stale-selection").await;
    let capped = ScriptedBackend::staged(|tree, _| {
        let session = tree.parent().unwrap().join("session/session.jsonl");
        std::fs::write(
            &session,
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
        )
        .unwrap();
        let mut result = support::done();
        result.exit_code = None;
        result.killed_reason = Some("cap".to_owned());
        result.session_file = Some(session.to_string_lossy().into_owned());
        result
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &capped, None)
        .await
        .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("suspended"));
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a cap suspension must resume");
    };
    let stale = f.store.get_finding(f.fid).await.unwrap().unwrap();
    f.store
        .set_finding_verdict(
            f.fid,
            hunter::domain::FindingStatus::Rejected,
            "not worth it",
        )
        .await
        .unwrap();

    let never = ScriptedBackend::new(|_| panic!("a rejected finding must not run"));
    let result = hunter::scheduler::run_fix(&f.store, &f.cfg, &stale, &never, Some(&plan))
        .await
        .unwrap();
    assert!(result.skipped.is_some());
    assert!(result.job_id.is_none());
    assert_eq!(
        result.kind,
        Some(hunter::domain::FindingJobKind::Fix.into()),
        "the cycle status line names the skipped kind"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "rejected");
    assert_eq!(finding.verdict_reason.as_deref(), Some("not worth it"));

    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the checkpoint must still resume");
    };
    assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
}
