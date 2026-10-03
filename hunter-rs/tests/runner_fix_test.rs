#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_fix` — worktree setup, and every way a fix attempt ends.
//!
//! The branch a fix job works on is named after the finding, so it is the
//! same name on every attempt. Any attempt that ends without deleting the
//! branch (the reclaim path only fires when the worktree *directory* is
//! still there) leaves `git worktree add -b` failing with "a branch named
//! ... already exists" — on that cycle and on every cycle after it, since
//! nothing about waiting changes it. Observed 2026-09-15: five
//! consecutive cycles two minutes apart, all for finding 3304, ended only
//! by restarting the daemon.
//!
//! The outcome tests run the real pipeline against real git: the worker
//! commits in the worktree it is handed, `git push` goes to the fixture's
//! bare origin (rerouted with `pushInsteadOf`, so the forge's ssh URL is
//! never dialled), and `gh` is a [`FakeBins`] script.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use hunter::backend::{Backend, JobClass, Outlook, Verdict};
use hunter::config::Config;
use hunter::domain::{FindingStatus, ForgeName};
use hunter::store::FindingInsert;
use hunter::types::RunResult;
use support::{EnvGuard, FakeBins, GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";
/// What `GitHubForge::ssh_url` makes of `REPO_URL`: the push target.
const PUSH_URL: &str = "git@github.com:acme/widget.git";
/// Matches the slug `run_fix` derives from the finding summary below.
const BRANCH: &str = "fix/a-real-bug-1";
const PR_TITLE: &str = "fix: stop the real bug";
/// Single line: the fake-binary log is tab/newline delimited.
const PR_BODY: &str = "Fixes the real bug.";
const PR_URL: &str = "https://github.com/acme/widget/pull/7";

struct Fixture {
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    repo_dir: std::path::PathBuf,
    origin: std::path::PathBuf,
    /// Last, so the directory outlives the Store's SQLite pool. See
    /// `runner_engage_test` for why, and for what it does not fix.
    _dir: TempDir,
}

/// A repo with a queued bug finding — the state `run_fix` expects.
async fn fixture(label: &str) -> Fixture {
    fixture_of(label, "bug").await
}

/// A repo with a queued finding of type `kind`.
async fn fixture_of(label: &str, kind: &str) -> Fixture {
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
            kind,
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
    std::fs::write(
        playbooks.join("apply_improvement.md"),
        "improve {{WORKTREE}}\n",
    )
    .unwrap();
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
// Outcome paths
// ---------------------------------------------------------------------------

/// The worktree a scripted worker was handed, recorded so a test can check
/// what became of it after `run_fix` returned.
type Seen = Arc<OnceLock<PathBuf>>;

/// What a scripted worker leaves in its worktree.
#[derive(Clone, Copy)]
enum Leaves {
    Nothing,
    /// One commit on the fix branch, no PR description.
    Commit,
    /// One commit plus `PR-DESCRIPTION.md`: everything a PR needs.
    CommitAndDescription,
}

/// A worker that leaves `leaves` behind and exits with `exit_code`.
fn worker(seen: &Seen, leaves: Leaves, exit_code: i32) -> ScriptedBackend {
    let seen = Arc::clone(seen);
    ScriptedBackend::new(move |tree| {
        seen.set(tree.to_path_buf()).unwrap();
        if matches!(leaves, Leaves::Commit | Leaves::CommitAndDescription) {
            std::fs::write(tree.join("fixed.rs"), "fixed\n").unwrap();
            git(tree, &["add", "fixed.rs"]);
            git(tree, &["commit", "-m", PR_TITLE]);
        }
        if matches!(leaves, Leaves::CommitAndDescription) {
            std::fs::write(tree.join("PR-DESCRIPTION.md"), PR_BODY).unwrap();
        }
        RunResult {
            exit_code: Some(exit_code),
            ..done()
        }
    })
}

fn seen() -> Seen {
    Arc::new(OnceLock::new())
}

fn tree_of(seen: &Seen) -> PathBuf {
    seen.get().cloned().expect("the worker ran")
}

/// Reroute `git push <PUSH_URL>` to `target` for the rest of the test.
/// A `target` that is not a repository makes every push fail.
fn route_push(bins: &FakeBins, target: &Path) -> [EnvGuard; 3] {
    let key = format!("url.{}.pushInsteadOf", target.display());
    [
        bins.env("GIT_CONFIG_COUNT", Path::new("1")),
        bins.env("GIT_CONFIG_KEY_0", Path::new(&key)),
        bins.env("GIT_CONFIG_VALUE_0", Path::new(PUSH_URL)),
    ]
}

async fn run(f: &Fixture, backend: &dyn Backend) -> hunter::scheduler::CycleSummary {
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, backend, None)
        .await
        .unwrap()
}

async fn stored(f: &Fixture) -> hunter::types::Finding {
    f.store.get_finding(f.fid).await.unwrap().unwrap()
}

fn branch_exists(repo_dir: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .args([
            "-C",
            &repo_dir.to_string_lossy(),
            "rev-parse",
            "--verify",
            "--quiet",
        ])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

#[tokio::test]
async fn a_finding_that_is_not_queued_is_skipped_untouched() {
    let f = fixture("fix-guard").await;
    f.store
        .set_finding_status(f.fid, FindingStatus::Rejected)
        .await
        .unwrap();

    let seen = seen();
    let summary = run(&f, &worker(&seen, Leaves::Nothing, 0)).await;

    assert!(summary.skipped.is_some(), "{summary:?}");
    assert!(seen.get().is_none(), "no worker may run");
    assert!(f.store.list_jobs(10).await.unwrap().is_empty());
    assert_eq!(stored(&f).await.status, FindingStatus::Rejected);
}

#[tokio::test]
async fn a_finding_whose_repo_is_gone_errors_and_logs_it() {
    let f = fixture("fix-no-repo").await;
    let mut finding = stored(&f).await;
    finding.repo_id = 9_999;

    let err =
        hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &ScriptedBackend::noop(), None)
            .await
            .expect_err("a missing repo is an error, not an outcome");

    assert!(err.to_string().contains("repo missing"), "{err}");
    let events = f.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("repo 9999 missing")),
        "{events:?}"
    );
    assert!(f.store.list_jobs(10).await.unwrap().is_empty());
}

/// `clean_repo_runs_the_fix` only checks the summary; this checks what the
/// daemon acts on next cycle: the stored verdict, and that the tree is gone.
#[tokio::test]
async fn not_a_bug_rejects_the_finding_with_the_workers_reason() {
    let f = fixture("fix-not-a-bug").await;
    let seen = seen();
    let seen2 = Arc::clone(&seen);
    let backend = ScriptedBackend::new(move |tree| {
        seen2.set(tree.to_path_buf()).unwrap();
        std::fs::write(tree.join("NOT-A-BUG.md"), "misread the code\nsecond line").unwrap();
        done()
    });

    let summary = run(&f, &backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("rejected"));
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Rejected);
    assert_eq!(
        finding.verdict_reason.as_deref(),
        Some("misread the code\nsecond line")
    );
    assert!(!tree_of(&seen).exists(), "the worktree must be released");
}

/// A non-bug finding declines with `DECLINED.md`, not `NOT-A-BUG.md`.
#[tokio::test]
async fn declined_rejects_a_non_bug_finding() {
    let f = fixture_of("fix-declined", "refactor").await;
    let backend = ScriptedBackend::writing("DECLINED.md", "not worth it");

    let summary = run(&f, &backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("rejected"), "{summary:?}");
    assert_eq!(summary.branch.as_deref(), Some("improve/a-real-bug-1"));
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Rejected);
    assert_eq!(finding.verdict_reason.as_deref(), Some("not worth it"));
}

/// The decline file is per kind: a bug worker's `DECLINED.md` is not a
/// verdict, so the attempt is an incomplete one.
#[tokio::test]
async fn declined_is_not_a_verdict_on_a_bug() {
    let f = fixture("fix-declined-bug").await;
    let backend = ScriptedBackend::writing("DECLINED.md", "not worth it");

    let summary = run(&f, &backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    assert_eq!(stored(&f).await.status, FindingStatus::Queued);
}

#[tokio::test]
async fn blocked_rejects_any_finding() {
    let f = fixture("fix-blocked").await;
    let backend = ScriptedBackend::writing("BLOCKED.md", "needs a schema decision");

    let summary = run(&f, &backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("rejected"));
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Rejected);
    assert_eq!(
        finding.verdict_reason.as_deref(),
        Some("needs a schema decision")
    );
    let events = f.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("blocked by worker")),
        "{events:?}"
    );
}

/// The whole happy path: pushed to the forge's ssh URL, PR opened with the
/// commit subject as title and the worker's description as body, finding
/// `pr_open`, streak cleared, tree released but the branch kept (it is the
/// PR's head).
#[tokio::test]
async fn commits_and_a_description_ship_a_draft_pr() {
    let f = fixture("fix-ship").await;
    let bins = FakeBins::acquire("fix-ship");
    bins.ok("gh", PR_URL);
    let _push = route_push(&bins, &f.origin);
    f.store
        .record_fix_attempt(f.fid, "no commits")
        .await
        .unwrap();
    f.store
        .set_budget_override(f.fid, Some("once"))
        .await
        .unwrap();

    let seen = seen();
    let summary = run(&f, &worker(&seen, Leaves::CommitAndDescription, 0)).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(summary.pr_url.as_deref(), Some(PR_URL));

    let pushed = git(&f.origin, &["log", "-1", "--format=%s", BRANCH]);
    assert_eq!(
        pushed.trim(),
        PR_TITLE,
        "the fix branch must reach the push target"
    );

    let creates = bins.calls_to("gh");
    assert_eq!(creates.len(), 1, "{creates:?}");
    let argv = &creates[0];
    let after = |flag: &str| {
        let i = argv.iter().position(|a| a == flag).unwrap();
        argv[i + 1].as_str()
    };
    assert_eq!(&argv[1..4], ["pr", "create", "--draft"]);
    assert_eq!(after("--head"), BRANCH);
    assert_eq!(after("--base"), "main");
    assert_eq!(after("--title"), PR_TITLE);
    assert_eq!(after("--body"), PR_BODY);

    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::PrOpen);
    assert_eq!(finding.pr_url.as_deref(), Some(PR_URL));
    assert_eq!(finding.fix_attempts, 0);
    assert_eq!(finding.last_fix_failure, None);
    assert_eq!(finding.budget_override, None, "`once` is spent by the PR");

    assert!(!tree_of(&seen).exists(), "the worktree must be released");
    assert!(
        branch_exists(&f.repo_dir, BRANCH),
        "the PR's head branch must survive"
    );
}

/// A PR left open by an earlier attempt: `gh` refuses with "already
/// exists" and names it, and the finding adopts that PR.
#[tokio::test]
async fn an_existing_pr_is_recovered_from_the_forge_error() {
    let f = fixture("fix-recover").await;
    let bins = FakeBins::acquire("fix-recover");
    bins.fail(
        "gh",
        1,
        &format!(
            "a pull request for branch \"{BRANCH}\" into branch \"main\" already exists:\n\
             https://github.com/acme/widget/pull/42"
        ),
    );
    let _push = route_push(&bins, &f.origin);
    f.store
        .set_budget_override(f.fid, Some("once"))
        .await
        .unwrap();

    let summary = run(&f, &worker(&seen(), Leaves::CommitAndDescription, 0)).await;

    let url = "https://github.com/acme/widget/pull/42";
    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(summary.pr_url.as_deref(), Some(url));
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::PrOpen);
    assert_eq!(finding.pr_url.as_deref(), Some(url));
    assert_eq!(finding.budget_override, None);
}

/// Any other forge refusal is an incomplete attempt, not a PR.
#[tokio::test]
async fn a_pr_the_forge_refuses_requeues_the_finding() {
    let f = fixture("fix-pr-refused").await;
    let bins = FakeBins::acquire("fix-pr-refused");
    bins.fail("gh", 1, "HTTP 422: Validation Failed");
    let _push = route_push(&bins, &f.origin);

    let summary = run(&f, &worker(&seen(), Leaves::CommitAndDescription, 0)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let failure = summary.failure.unwrap();
    assert!(failure.starts_with("PR create failed: "), "{failure}");
    assert!(failure.contains("Validation Failed"), "{failure}");
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Queued);
    assert_eq!(finding.pr_url, None);
}

#[tokio::test]
async fn a_failed_push_requeues_without_opening_a_pr() {
    let f = fixture("fix-push-fails").await;
    let bins = FakeBins::acquire("fix-push-fails");
    bins.ok("gh", PR_URL);
    let nowhere = f.origin.with_file_name("no-such-remote.git");
    let _push = route_push(&bins, &nowhere);

    let summary = run(&f, &worker(&seen(), Leaves::CommitAndDescription, 0)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let failure = summary.failure.unwrap();
    assert!(failure.starts_with("push failed: "), "{failure}");
    assert!(
        bins.calls_to("gh").is_empty(),
        "no PR for an unpushed branch"
    );
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Queued);
    assert_eq!(finding.fix_attempts, 1);
    assert_eq!(finding.last_fix_failure.as_deref(), Some(failure.as_str()));
}

#[tokio::test]
async fn a_worker_that_commits_nothing_is_requeued() {
    let f = fixture("fix-no-commits").await;
    let summary = run(&f, &worker(&seen(), Leaves::Nothing, 0)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    assert_eq!(summary.failure.as_deref(), Some("no commits"));
    assert_eq!(stored(&f).await.status, FindingStatus::Queued);
}

#[tokio::test]
async fn commits_without_a_description_are_requeued() {
    let f = fixture("fix-no-description").await;
    let summary = run(&f, &worker(&seen(), Leaves::Commit, 0)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    assert_eq!(summary.failure.as_deref(), Some("no PR-DESCRIPTION.md"));
}

/// A worker that did not finish is blamed on the worker, even if it left
/// something behind: the missing-commits wording is for workers that ended
/// cleanly.
#[tokio::test]
async fn a_worker_that_failed_is_reported_by_its_state() {
    let f = fixture("fix-worker-failed").await;
    let summary = run(&f, &worker(&seen(), Leaves::Commit, 1)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    assert_eq!(summary.failure.as_deref(), Some("worker failed"));
}

/// The third identical failure in a row gives up on the finding.
#[tokio::test]
async fn the_third_identical_failure_rejects_the_finding_as_stuck() {
    let f = fixture("fix-stuck").await;
    for attempt in 1..=2 {
        let summary = run(&f, &worker(&seen(), Leaves::Nothing, 0)).await;
        assert_eq!(
            summary.outcome.as_deref(),
            Some("requeued"),
            "attempt {attempt}"
        );
        assert_eq!(stored(&f).await.fix_attempts, attempt);
    }
    f.store
        .set_budget_override(f.fid, Some("once"))
        .await
        .unwrap();

    let summary = run(&f, &worker(&seen(), Leaves::Nothing, 0)).await;

    assert_eq!(summary.outcome.as_deref(), Some("stuck"), "{summary:?}");
    assert_eq!(summary.attempts, Some(3));
    assert_eq!(summary.failure.as_deref(), Some("no commits"));
    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Rejected);
    let reason = finding.verdict_reason.unwrap();
    assert!(
        reason.starts_with("stuck: 3 consecutive fix attempts"),
        "{reason}"
    );
    assert_eq!(
        finding.fix_attempts, 0,
        "the streak is cleared with the verdict"
    );
    assert_eq!(finding.budget_override, None);
}

/// Only an unbroken run of the same failure counts: a different failure
/// in between restarts the streak.
#[tokio::test]
async fn a_different_failure_resets_the_streak() {
    let f = fixture("fix-streak-reset").await;
    for leaves in [
        Leaves::Nothing,
        Leaves::Nothing,
        Leaves::Commit,
        Leaves::Nothing,
    ] {
        let summary = run(&f, &worker(&seen(), leaves, 0)).await;
        assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    }

    let finding = stored(&f).await;
    assert_eq!(finding.status, FindingStatus::Queued);
    assert_eq!(finding.fix_attempts, 1);
    assert_eq!(finding.last_fix_failure.as_deref(), Some("no commits"));
}

/// `once` is spent by any attempt that ran; `exempt` persists.
#[tokio::test]
async fn a_requeue_spends_a_once_override_but_not_an_exempt_one() {
    for (mode, after) in [("once", None), ("exempt", Some("exempt"))] {
        let f = fixture(&format!("fix-override-{mode}")).await;
        f.store
            .set_budget_override(f.fid, Some(mode))
            .await
            .unwrap();

        let summary = run(&f, &worker(&seen(), Leaves::Nothing, 0)).await;

        assert_eq!(summary.outcome.as_deref(), Some("requeued"));
        assert_eq!(stored(&f).await.budget_override.as_deref(), after, "{mode}");
    }
}

// ---------------------------------------------------------------------------
// Before the worker produces anything
// ---------------------------------------------------------------------------

/// A backend that denies the job, or grants it and then fails to run it.
struct Refusing {
    deny: bool,
}

#[async_trait::async_trait]
impl Backend for Refusing {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let verdict = if self.deny {
            Verdict::Denied {
                reason: "test: window exhausted".to_owned(),
                retry_at: None,
            }
        } else {
            Verdict::Granted {
                cap_tokens: None,
                reason: "test: granted".to_owned(),
            }
        };
        Ok(Outlook {
            normal: verdict.clone(),
            prioritized: verdict,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn run(
        &self,
        _ws: &hunter::workspace::Workspace,
        _prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        _resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        anyhow::bail!("test: worker could not be spawned")
    }
}

#[tokio::test]
async fn a_denied_budget_starts_nothing() {
    let f = fixture("fix-denied").await;

    let summary = run(&f, &Refusing { deny: true }).await;

    assert_eq!(summary.denied.as_deref(), Some("test: window exhausted"));
    assert!(f.store.list_jobs(10).await.unwrap().is_empty());
    assert!(
        !branch_exists(&f.repo_dir, BRANCH),
        "no branch for a denied job"
    );
    assert_eq!(stored(&f).await.status, FindingStatus::Queued);
}

/// A backend error must not strand the finding in `fixing`, where no tier
/// would ever pick it up again.
#[tokio::test]
async fn a_backend_error_returns_the_finding_to_queued() {
    let f = fixture("fix-backend-error").await;
    let finding = stored(&f).await;

    let err =
        hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &Refusing { deny: false }, None)
            .await
            .expect_err("a backend error propagates");

    assert!(err.to_string().contains("could not be spawned"), "{err}");
    assert_eq!(stored(&f).await.status, FindingStatus::Queued);
}
