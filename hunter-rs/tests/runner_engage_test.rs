#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_engage` — the withdrawal path — and what a PR closing does to its
//! finding.
//!
//! This is the runner layer, which had no tests until a review found a P1
//! here: `close_pr` was changed to propagate a failed withdrawal comment,
//! but the caller still recorded the withdrawal as complete, leaving the
//! PR open on the forge, closed in the database, and the finding rejected
//! — and `sync_prs` only revisits `pr_open` findings, so nothing would
//! ever reconcile it.
//!
//! A closed PR, whether a worker withdrew it or a human closed it, leaves
//! its finding `closed` for the harvest to classify, never `rejected`:
//! 15 of the first 16 closures were engage withdrawals, 14 of them
//! "superseded/obsolete", and rejecting those suppressed valid findings.
//!
//! Worth reading as a template for the other runners: `support` gives you
//! a git repo with a real origin, a scripted `gh`, and a backend that
//! stages whatever the worker would have left in the worktree.

mod support;

use hunter::config::Config;
use hunter::domain::{FindingStatus, ForgeName, RepoJobKind};
use hunter::store::{FindingInsert, SyncPrData};
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, fresh_store};

const BRANCH: &str = "fix/some-bug";
const PR_URL: &str = "https://github.com/acme/widget/pull/7";
const REPO_URL: &str = "https://github.com/acme/widget";

/// Minimal `gh pr view --json` payload that `view_pr_engage` can parse.
const PR_VIEW_JSON: &str = r#"{"state":"OPEN","mergeable":"MERGEABLE","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;

/// The same PR as `sync_prs` sees it once a human has closed it.
const PR_CLOSED_JSON: &str = r#"{"state":"CLOSED","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;

/// One valid follow-up entry, so a follow-up that is not filed is known
/// to have been skipped rather than rejected by ingest.
const FOLLOW_UP: &str = r#"[{"type":"bug","fingerprint":"widget:src/lib.rs:left-open","file":"src/lib.rs","bug_class":"logic","severity":"medium","confidence":0.8,"summary":"left open","detail":"src/lib.rs:1 still has it","evidence_plan":"failing test"}]"#;

struct Fixture {
    db: std::path::PathBuf,
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    /// Last, because fields drop in declaration order and the directory
    /// has to outlive the Store's SQLite pool -- the same rule `fresh_db`
    /// documents and `TestState` follows.
    ///
    /// Measured, so the comment does not overclaim: on Linux the old
    /// order leaks nothing, because unlinking a file an open handle
    /// still holds is legal and `remove_dir_all` succeeds. What the
    /// order buys here is that the pool is never writing into a
    /// directory that has already been removed, and one rule across
    /// every fixture instead of two -- not a leak fix on this platform.
    _dir: TempDir,
}

impl Fixture {
    /// `pr_state.addressed_fingerprint` — set when an engage cycle has
    /// taken its shot at the current attention reason.
    async fn addressed_fp(&self) -> Option<String> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT addressed_fingerprint FROM pr_state WHERE finding_id = ?1")
                .bind(self.fid)
                .fetch_optional(&pool)
                .await
                .unwrap();
        row.and_then(|r| r.0)
    }

    /// `pr_state.state` read straight from the database — the runner's
    /// own Store API exposes no reader for it, and the point of the test
    /// is what was persisted.
    async fn pr_state(&self) -> Option<String> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(String,)> =
            sqlx::query_as("SELECT state FROM pr_state WHERE finding_id = ?1")
                .bind(self.fid)
                .fetch_optional(&pool)
                .await
                .unwrap();
        row.map(|r| r.0)
    }
}

/// A repo with a published branch, a `pr_open` finding, and `pr_state`
/// flagged for attention — the state `run_engage` expects to be handed.
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (db, store) = fresh_store(&dir, "engage").await;

    // The clone location is the store's to choose (`repos/repo-<id>`), so
    // the fixture's git repo moves to wherever the id lands rather than the
    // row being pointed at the fixture.
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
                fingerprint: "fp-engage-1".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: "medium".to_owned(),
                confidence: 0.9,
                summary: "a fix".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_pr_open(fid, PR_URL).await.unwrap();
    store
        .sync_pr_open(
            fid,
            &SyncPrData {
                pr_number: 7,
                state: "open".to_owned(),
                mergeable: "mergeable".to_owned(),
                checks: None,
                head_ref: BRANCH.to_owned(),
                head_sha: "deadbeef".to_owned(),
                last_activity_at: 1,
                last_engaged_activity_at: 0,
                needs_attention: Some("reviewer asked a question".to_owned()),
                attention_fingerprint: Some("af-1".to_owned()),
                synced_at: 1,
                attention_since: Some(Some(1)),
                clear_addressed: false,
            },
        )
        .await
        .unwrap();

    // Hermetic: the playbook is a stub written into the scratch root, not
    // the repo's real one. Reading `../hunter` made the test depend on the
    // developer's checkout layout — it broke the moment cargo-mutants ran
    // it from a temp copy — and coupled it to prompt wording it does not
    // care about. The scripted worker ignores the prompt entirely; all
    // that matters is that rendering succeeds, so the stub uses one slot
    // the engage builder is known to supply.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("engage.md"), "engage {{WORKTREE}}\n").unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        _dir: dir,
        db,
        cfg,
        store,
        fid,
    }
}

/// A withdrawal is only real once the forge has accepted it. If the
/// comment fails, `close_pr` never issues the close — so recording the
/// verdict anyway would strand an open PR that nothing revisits.
#[tokio::test]
async fn failed_close_does_not_record_the_withdrawal() {
    let f = fixture("engage-close-fails").await;
    let bins = FakeBins::acquire("engage-close-fails");
    // `gh pr view` succeeds; `gh pr comment` fails; `gh pr close` must
    // therefore never run.
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("WITHDRAW.md", "not worth pursuing");
    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .expect("run_engage should not error, it should decline to record");

    assert_eq!(summary.outcome.as_deref(), Some("withdraw-failed"));
    assert!(
        !bins.called_with("gh", "close"),
        "close must not be issued when the comment failed: {:?}",
        bins.calls()
    );

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        FindingStatus::PrOpen,
        "finding must stay pr_open so sync_prs keeps revisiting it"
    );
    assert!(
        after.verdict_reason.is_none(),
        "no verdict may be recorded for a withdrawal the forge rejected"
    );
    assert_eq!(
        f.pr_state().await.as_deref(),
        Some("open"),
        "pr_state must keep the value sync wrote, not be flipped to closed"
    );
}

/// The happy path, so the test above is known to be asserting on a
/// difference rather than on a runner that always declines. A withdrawal
/// closes the PR and leaves the finding `closed`, awaiting its harvest,
/// with the worker's reason; it does not suppress it.
#[tokio::test]
async fn successful_close_records_the_withdrawal() {
    let f = fixture("engage-close-ok").await;
    let bins = FakeBins::acquire("engage-close-ok");
    bins.ok("gh", PR_VIEW_JSON);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("WITHDRAW.md", "superseded by #9");
    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    assert!(
        bins.called_with("gh", "close"),
        "close must be issued once the comment landed: {:?}",
        bins.calls()
    );

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(after.verdict_reason.as_deref(), Some("superseded by #9"));
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let suppressed = f.store.suppressions(after.repo_id, "bug").await.unwrap();
    assert!(
        suppressed.is_empty(),
        "a withdrawn finding must not feed the suppression corpus: {suppressed:?}"
    );
}

/// A withdrawal files no follow-ups: the harvest of the closed PR owns
/// them now. Ingesting the withdrawing worker's `FOLLOW-UPS.json` as well
/// would file the same work twice, under two different provenances.
#[tokio::test]
async fn a_withdrawal_files_no_follow_ups() {
    let f = fixture("engage-withdraw-no-followups").await;
    let bins = FakeBins::acquire("engage-withdraw-no-followups");
    bins.ok("gh", PR_VIEW_JSON);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        support::done()
    });
    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    assert!(summary.ingest.is_none(), "{summary:?}");
    let all = f
        .store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|x| x.id).collect::<Vec<_>>(),
        vec![f.fid],
        "no follow-up may be filed by the withdrawal"
    );
}

/// A PR closed on the forge (here: by a human) leaves its finding
/// `closed`, awaiting the harvest, and out of the suppression corpus.
/// Rejecting it on sight is what silenced valid findings: a closure alone
/// does not say whether the finding was wrong.
#[tokio::test]
async fn a_pr_closed_on_the_forge_waits_for_its_harvest() {
    let f = fixture("sync-closed").await;
    let bins = FakeBins::acquire("sync-closed");
    bins.ok("gh", PR_CLOSED_JSON);

    let result = hunter::scheduler::sync_prs(&f.store, &f.cfg).await;

    assert_eq!(result.closed, 1, "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some("PR closed without merge; awaiting harvest")
    );
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let suppressed = f.store.suppressions(after.repo_id, "bug").await.unwrap();
    assert!(
        suppressed.is_empty(),
        "a closed PR's finding must not feed the suppression corpus: {suppressed:?}"
    );
}

/// A closed PR whose closure cannot be recorded keeps its finding
/// `pr_open`. `sync_prs` only revisits `pr_open` findings, so setting it
/// `closed` over a `pr_state` still reading OPEN would strand it; left
/// `pr_open`, the next sync records both.
#[tokio::test]
async fn a_closure_that_cannot_be_recorded_is_retried_by_the_next_sync() {
    let f = fixture("sync-closed-unrecorded").await;
    let bins = FakeBins::acquire("sync-closed-unrecorded");
    bins.ok("gh", PR_CLOSED_JSON);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_close BEFORE UPDATE OF state ON pr_state \
         WHEN NEW.state = 'CLOSED' \
         BEGIN SELECT RAISE(ABORT, 'injected mark_pr_closed failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = hunter::scheduler::sync_prs(&f.store, &f.cfg).await;

    assert_eq!(result.closed, 0, "{result:?}");
    assert_eq!(result.errors, 1, "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_ne!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("PR closed but not recorded")),
        "{events:?}"
    );

    sqlx::query("DROP TRIGGER no_close")
        .execute(&pool)
        .await
        .unwrap();
    let result = hunter::scheduler::sync_prs(&f.store, &f.cfg).await;

    assert_eq!(result.closed, 1, "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
}

/// A failed withdrawal must not become an unbounded retry.
///
/// Leaving the finding untouched puts it straight back into
/// `list_attention`, which `pick_next` ranks second — so a persistently
/// failing forge would run a fresh worker every cycle forever, starving
/// every other kind of work. `fix`, `recheck` and `harvest` each cap this
/// with an attempt counter; engage has none, so the attention reason is
/// marked addressed instead and the finding waits for real PR activity.
#[tokio::test]
async fn failed_close_marks_the_attention_addressed_so_it_stops_being_repicked() {
    let f = fixture("engage-close-fails-bounded").await;
    let bins = FakeBins::acquire("engage-close-fails-bounded");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);

    assert_eq!(
        f.addressed_fp().await,
        None,
        "precondition: not yet addressed"
    );

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("WITHDRAW.md", "not worth pursuing");
    hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(
        f.addressed_fp().await.as_deref(),
        Some("af-1"),
        "the attention reason must be marked addressed, or the next cycle picks it straight back up"
    );
}

/// A clone directory belonging to a different repository is refused.
///
/// `repos/repo-<id>` is derived from the id, so a delete whose directory
/// removal failed — or anything an operator put there — leaves a clone at
/// the path a later repo would use, and it would otherwise be used as-is:
/// `sync_repo` skips cloning whenever the path exists, so the daemon would
/// hunt one project's code, file the findings against another, and push to
/// whichever URL the database named.
#[tokio::test]
async fn a_clone_of_a_different_repository_is_refused() {
    let dir = TempDir::new("wrong-origin");
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (_db, store) = fresh_store(&dir, "origin").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    // Registered under a URL that is *not* the origin of the checkout
    // sitting at its path — exactly the state a leftover directory leaves.
    let rid = store
        .add_repo(
            "widget",
            "https://github.com/acme/somebody-elses-repo",
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    let repo_dir = hunter::store::Store::repo_dir(&repos_root, rid);
    std::fs::rename(&repo.work, &repo_dir).unwrap();

    let row = store.get_repo_by_id(rid).await.unwrap().unwrap();
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("hunt.md"), "hunt {{WORKTREE}}\n").unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    // Never reached: the sync refuses before any worker is spawned, which
    // is itself part of the contract.
    let backend = ScriptedBackend::writing("findings.json", "[]");
    let err = hunter::scheduler::run_hunt(&store, &cfg, &row, &backend, None)
        .await
        .expect_err("a clone of another repository must not be hunted");
    let msg = err.to_string();
    assert!(
        msg.contains("but its origin is") && msg.contains("somebody-elses-repo"),
        "the refusal must name both URLs so the operator can see the mismatch: {msg}"
    );
}

/// A hunt whose repo was deleted between the pick and the job insert
/// reports which kind and which repo it skipped.
///
/// `create_job` refuses a soft-deleted repo, and the executor turns that
/// into a skipped cycle rather than an error: deleting a repo is operator
/// traffic, not a crash. The status page and the cycle log render that
/// summary, so "skipped" without the kind and repo would tell the
/// operator nothing about what was dropped. The row handed to `run_hunt`
/// is the one the pick read, taken before the delete, exactly as a cycle
/// holds it.
#[tokio::test]
async fn a_hunt_refused_because_its_repo_was_deleted_names_what_it_skipped() {
    let dir = TempDir::new("refused-hunt");
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (_db, store) = fresh_store(&dir, "refused").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            &repo.origin.to_string_lossy(),
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    std::fs::rename(&repo.work, hunter::store::Store::repo_dir(&repos_root, rid)).unwrap();

    let picked = store.get_repo_by_id(rid).await.unwrap().unwrap();
    store.soft_delete_repo(rid).await.unwrap();

    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("hunt.md"), "hunt {{WORKTREE}}\n").unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    // Never reached: the job row is refused before any worker is spawned.
    let backend = ScriptedBackend::writing("findings.json", "[]");
    let summary = hunter::scheduler::run_hunt(&store, &cfg, &picked, &backend, None)
        .await
        .expect("losing the race to a delete is a skipped cycle, not an error");

    assert_eq!(
        (
            summary.kind,
            summary.repo.as_deref(),
            summary.finding_id,
            summary.skipped.as_deref(),
        ),
        (
            Some(RepoJobKind::Hunt.into()),
            Some("widget"),
            None,
            Some(format!("repo {rid} is deleted -- cannot start a hunt job").as_str()),
        ),
        "a refused job must still name the kind and repo it skipped: {summary:?}"
    );
    assert_eq!(summary.job_id, None, "no job row may have been written");
}
