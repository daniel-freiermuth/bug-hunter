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
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";
/// Matches the slug `run_fix` derives from the finding summary below.
const BRANCH: &str = "fix/a-real-bug-1";

struct Fixture {
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    repo_dir: std::path::PathBuf,
    /// The bare remote the repo was cloned from.
    origin: std::path::PathBuf,
    /// Last, so the directory outlives the Store's SQLite pool. See
    /// `runner_engage_test` for why, and for what it does not fix.
    _dir: TempDir,
}

/// A repo with a queued bug finding — the state `run_fix` expects.
async fn fixture(label: &str) -> Fixture {
    fixture_on(label, REPO_URL, ForgeName::Github).await
}

/// [`fixture`] for a repo registered under `url` on `forge`.
async fn fixture_on(label: &str, url: &str, forge: ForgeName) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "some-other-branch");
    let (_db, store) = fresh_store(&dir, "fix").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo("widget", url, &repos_root, &repo.default_branch, forge)
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
// Recovering a PR that already exists
// ---------------------------------------------------------------------------

const GL_REPO_URL: &str = "https://gitlab.com/group/widget";
const GL_MR_URL: &str = "https://gitlab.com/group/widget/-/merge_requests/42";

/// What `glab mr create` (1.119) prints when GitLab answers 409 because
/// the branch already has an open MR. Captured against a stub API, host
/// rewritten. It carries no MR web URL — only `!42` — and glab wraps it
/// at a fixed width, which splits "already exists" for some project
/// paths.
const GLAB_MR_EXISTS: &str = "\nCreating draft merge request for fix/a-real-bug-1 into main in group/widget\n\nFailed to create merge request. Created recovery file: /tmp/glab/recover/group/widget/mr.json\nRun the command again with the '--recover' option to retry.\n          \n   ERROR  \n          \n  Post https://gitlab.com/api/v4/projects/group%2Fwidget/merge_requests: 409 {message: [Another open merge request already\n  exists for this source branch: !42]}.";

/// A worker that commits a change and leaves a PR description: the run
/// goes on to push and open the PR.
fn committing_worker() -> ScriptedBackend {
    ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("fix.txt"), "fixed\n").expect("stage fix");
        git(tree, &["add", "fix.txt"]);
        git(tree, &["commit", "-m", "fix: a real bug"]);
        std::fs::write(tree.join("PR-DESCRIPTION.md"), "what broke\n").expect("stage PR body");
        done()
    })
}

/// A retried fix whose MR is already open on GitLab adopts that MR, as a
/// GitHub retry adopts its PR. Failing instead repeats the identical
/// "PR create failed" on every retry until the streak rejects a finding
/// whose fix is sitting in an open MR.
#[tokio::test]
async fn gitlab_retry_adopts_the_mr_that_already_exists() {
    let f = fixture_on("fix-gl-exists", GL_REPO_URL, ForgeName::Gitlab).await;
    let bins = FakeBins::acquire("fix-gl-exists");
    // The push goes to git@gitlab.com:group/widget.git; land it on the
    // local bare origin instead.
    let insteadof = format!("url.{}.insteadOf", f.origin.display());
    let _count = bins.env("GIT_CONFIG_COUNT", std::path::Path::new("1"));
    let _key = bins.env("GIT_CONFIG_KEY_0", std::path::Path::new(&insteadof));
    let _value = bins.env(
        "GIT_CONFIG_VALUE_0",
        std::path::Path::new("git@gitlab.com:group/widget.git"),
    );
    bins.script(
        "glab",
        &format!(
            "case \"$1 $2\" in\n\"mr create\") cat >&2 <<'__FAKE_EOF__'\n{GLAB_MR_EXISTS}\n__FAKE_EOF__\nexit 1;;\n\"mr list\") echo '{GL_MR_URL}'; exit 0;;\nesac\nexit 1"
        ),
    );

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary =
        hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &committing_worker(), None)
            .await
            .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(summary.pr_url.as_deref(), Some(GL_MR_URL));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status, hunter::domain::FindingStatus::PrOpen);
    assert_eq!(finding.pr_url.as_deref(), Some(GL_MR_URL));
}
