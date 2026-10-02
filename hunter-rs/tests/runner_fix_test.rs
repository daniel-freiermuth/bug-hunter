#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_fix` — worktree setup, and what a run does with what the worker
//! left behind: ship it (push, draft PR), recover a PR that already
//! exists, or salvage the attempt (requeue, then give up as stuck).
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

use std::path::Path;

use hunter::config::Config;
use hunter::domain::{FindingStatus, ForgeName};
use hunter::store::FindingInsert;
use support::{EnvGuard, FakeBins, GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";
/// Matches the slug `run_fix` derives from the finding summary below.
const BRANCH: &str = "fix/a-real-bug-1";

struct Fixture {
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    repo_dir: std::path::PathBuf,
    /// The bare repository `repo_dir` was cloned from.
    origin: std::path::PathBuf,
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
        origin: repo.origin,
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

// ---------------------------------------------------------------------------
// What the run does with the worker's output
// ---------------------------------------------------------------------------

/// The finding as the store has it now.
async fn reload(f: &Fixture) -> hunter::types::Finding {
    f.store.get_finding(f.fid).await.unwrap().unwrap()
}

/// Run one fix attempt of the fixture's finding, as the store has it now.
async fn fix(f: &Fixture, backend: &ScriptedBackend) -> hunter::scheduler::CycleSummary {
    let finding = reload(f).await;
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, backend, None)
        .await
        .unwrap()
}

/// Send `run_fix`'s push, which targets the forge's SSH URL, to `target`
/// instead: git's own `url.<base>.insteadOf`, scoped through the
/// environment so real git still does the pushing.
fn route_push(bins: &FakeBins, target: &Path) -> [EnvGuard; 3] {
    let key = format!("url.{}.insteadOf", target.display());
    [
        bins.env("GIT_CONFIG_COUNT", Path::new("1")),
        bins.env("GIT_CONFIG_KEY_0", Path::new(&key)),
        bins.env(
            "GIT_CONFIG_VALUE_0",
            Path::new("git@github.com:acme/widget.git"),
        ),
    ]
}

/// A worker that commits a change and, if `describe`, writes the PR body.
fn committing(describe: bool) -> ScriptedBackend {
    ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
        git(tree, &["add", "-A"]);
        git(tree, &["commit", "-m", "fix: the real bug"]);
        if describe {
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
        }
        done()
    })
}

/// A finding that is not queued is skipped before anything is created:
/// no job row, no workspace, no status change.
#[tokio::test]
async fn a_finding_that_is_not_queued_is_skipped() {
    let f = fixture("fix-not-queued").await;
    f.store
        .set_finding_status(f.fid, FindingStatus::New)
        .await
        .unwrap();

    let backend = ScriptedBackend::noop();
    let summary = fix(&f, &backend).await;

    assert!(
        summary
            .skipped
            .as_deref()
            .is_some_and(|s| s.contains("not queued")),
        "{summary:?}"
    );
    assert!(backend.runs().is_empty(), "no worker may run");
    assert!(f.store.list_jobs(10).await.unwrap().is_empty());
    assert!(!f.cfg.work_root.join("jobs").exists(), "no workspace");
    assert_eq!(reload(&f).await.status, FindingStatus::New);
}

/// The ship path: commits plus a PR description are pushed to the forge,
/// a draft PR is opened for the branch, and the finding moves to
/// `pr_open` with the URL `gh` printed and a clean failure streak.
#[tokio::test]
async fn committed_and_described_work_is_pushed_and_opened_as_a_pr() {
    let bins = FakeBins::acquire("fix-ship");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = fixture("fix-ship").await;
    let _push = route_push(&bins, &f.origin);
    f.store
        .record_fix_attempt(f.fid, "no commits")
        .await
        .unwrap();

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(
        summary.pr_url.as_deref(),
        Some("https://github.com/acme/widget/pull/42")
    );
    let after = reload(&f).await;
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_eq!(
        after.pr_url.as_deref(),
        Some("https://github.com/acme/widget/pull/42")
    );
    assert_eq!(after.fix_attempts, 0, "a shipped fix ends the streak");
    assert_eq!(after.last_fix_failure, None);

    let pushed = git(&f.origin, &["log", "-1", "--format=%s", BRANCH]);
    assert_eq!(
        pushed.trim(),
        "fix: the real bug",
        "the branch reached origin"
    );
    let creates = bins.calls_to("gh");
    assert_eq!(creates.len(), 1, "{creates:?}");
    let call = &creates[0];
    assert_eq!(&call[1..4], ["pr", "create", "--draft"], "{call:?}");
    let after_flag = |flag: &str| {
        let i = call.iter().position(|a| a == flag).unwrap();
        call[i + 1].clone()
    };
    assert_eq!(after_flag("--head"), BRANCH);
    assert_eq!(after_flag("--base"), "main");
    assert_eq!(
        after_flag("--title"),
        "fix: the real bug",
        "last commit subject"
    );
    assert_eq!(after_flag("--body"), "the body");
}

/// `gh pr create` refusing because a PR for the branch already exists is
/// not a failure: that PR is the fix, recovered from the error text.
#[tokio::test]
async fn an_already_existing_pr_is_recovered() {
    let bins = FakeBins::acquire("fix-pr-exists");
    bins.fail(
        "gh",
        1,
        "a pull request for branch \"fix/a-real-bug-1\" into branch \"main\" already exists:\nhttps://github.com/acme/widget/pull/7",
    );
    let f = fixture("fix-pr-exists").await;
    let _push = route_push(&bins, &f.origin);

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(
        summary.pr_url.as_deref(),
        Some("https://github.com/acme/widget/pull/7")
    );
    let after = reload(&f).await;
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_eq!(
        after.pr_url.as_deref(),
        Some("https://github.com/acme/widget/pull/7")
    );
}

/// A PR-create failure with no PR to recover requeues the finding and
/// counts toward the streak.
#[tokio::test]
async fn a_failed_pr_create_requeues() {
    let bins = FakeBins::acquire("fix-pr-fails");
    bins.fail("gh", 1, "HTTP 422: Validation Failed");
    let f = fixture("fix-pr-fails").await;
    let _push = route_push(&bins, &f.origin);

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let failure = summary.failure.unwrap();
    assert!(failure.starts_with("PR create failed"), "{failure}");
    assert!(failure.contains("Validation Failed"), "{failure}");
    let after = reload(&f).await;
    assert_eq!(after.status, FindingStatus::Queued);
    assert_eq!(after.pr_url, None);
    assert_eq!(after.fix_attempts, 1);
    assert_eq!(after.last_fix_failure.as_deref(), Some(failure.as_str()));
}

/// A push that fails requeues the finding, and no PR is attempted for a
/// branch the forge does not have.
#[tokio::test]
async fn a_failed_push_requeues_without_creating_a_pr() {
    let bins = FakeBins::acquire("fix-push-fails");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = fixture("fix-push-fails").await;
    let _push = route_push(&bins, &f.origin.with_file_name("no-such-remote.git"));

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let failure = summary.failure.unwrap();
    assert!(failure.starts_with("push failed"), "{failure}");
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    assert_eq!(reload(&f).await.status, FindingStatus::Queued);
}

/// Work that cannot be shipped is requeued, named by what is missing:
/// a worker that committed nothing, one that committed but wrote no PR
/// description, and one that did not finish.
#[tokio::test]
async fn unshippable_work_is_requeued_with_the_reason() {
    let killed = ScriptedBackend::new(|_| hunter::types::RunResult {
        exit_code: None,
        killed_reason: Some("wallclock".to_owned()),
        ..done()
    });
    for (label, backend, reason) in [
        ("fix-no-commits", ScriptedBackend::noop(), "no commits"),
        (
            "fix-no-description",
            committing(false),
            "no PR-DESCRIPTION.md",
        ),
        ("fix-worker-killed", killed, "worker killed"),
    ] {
        let f = fixture(label).await;

        let summary = fix(&f, &backend).await;

        assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
        assert_eq!(summary.failure.as_deref(), Some(reason), "{label}");
        let after = reload(&f).await;
        assert_eq!(after.status, FindingStatus::Queued, "{label}");
        assert_eq!(after.last_fix_failure.as_deref(), Some(reason), "{label}");
    }
}

/// The same failure three times running is a fix that will not converge:
/// the third attempt rejects the finding as stuck and resets the streak.
#[tokio::test]
async fn three_identical_failures_reject_the_finding_as_stuck() {
    let f = fixture("fix-stuck").await;
    let backend = ScriptedBackend::noop();

    for attempt in 1..=2 {
        let summary = fix(&f, &backend).await;
        assert_eq!(
            summary.outcome.as_deref(),
            Some("requeued"),
            "attempt {attempt}: {summary:?}"
        );
        assert_eq!(reload(&f).await.fix_attempts, attempt);
    }
    let summary = fix(&f, &backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("stuck"), "{summary:?}");
    assert_eq!(summary.attempts, Some(3));
    assert_eq!(summary.failure.as_deref(), Some("no commits"));
    let after = reload(&f).await;
    assert_eq!(after.status, FindingStatus::Rejected);
    let verdict = after.verdict_reason.unwrap_or_default();
    assert!(verdict.starts_with("stuck:"), "{verdict}");
    assert!(verdict.contains("no commits"), "{verdict}");
    assert_eq!(after.fix_attempts, 0);
    assert_eq!(after.last_fix_failure, None);
}

/// A different failure in between restarts the streak, so alternating
/// failures are retried rather than declared stuck.
#[tokio::test]
async fn a_different_failure_restarts_the_streak() {
    let f = fixture("fix-streak-reset").await;
    f.store
        .record_fix_attempt(f.fid, "no commits")
        .await
        .unwrap();
    f.store
        .record_fix_attempt(f.fid, "no commits")
        .await
        .unwrap();

    let summary = fix(&f, &committing(false)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let after = reload(&f).await;
    assert_eq!(after.status, FindingStatus::Queued);
    assert_eq!(after.fix_attempts, 1);
}

/// Every end of an attempt spends a `once` budget override; an `exempt`
/// one lasts until a human clears it.
#[tokio::test]
async fn only_a_once_override_is_spent_by_a_shipped_or_requeued_fix() {
    let bins = FakeBins::acquire("fix-override");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    for (mode, left) in [("once", None), ("exempt", Some("exempt"))] {
        for (end, describe) in [("pr_open", true), ("requeued", false)] {
            let f = fixture(&format!("fix-override-{mode}-{end}")).await;
            let _push = route_push(&bins, &f.origin);
            f.store
                .set_budget_override(f.fid, Some(mode))
                .await
                .unwrap();

            let summary = fix(&f, &committing(describe)).await;

            assert_eq!(summary.outcome.as_deref(), Some(end), "{mode}: {summary:?}");
            let after = reload(&f).await;
            assert_eq!(
                after.budget_override.as_deref(),
                left,
                "{mode} override after {end}"
            );
        }
    }
}
