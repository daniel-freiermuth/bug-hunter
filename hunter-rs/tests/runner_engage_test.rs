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

/// The closed PR's diff, as `gh pr diff` prints it.
const PR_DIFF: &str = "diff --git a/CHANGE.md b/CHANGE.md\n+change\n";

/// One usage record in omp's session format: a 100,000-token context, so
/// the handoff reserves 100,000 + max(0, 100,000) = 200,000 under the
/// resume rule, against engage's cold 40,000.
const USAGE: &str = r#"{"message":{"role":"assistant","usage":{"input":100000,"output":100,"cacheRead":0,"cacheWrite":0}}}"#;

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

    /// `pr_state.needs_attention` as `sync_prs` persisted it.
    async fn attention(&self) -> Option<String> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT needs_attention FROM pr_state WHERE finding_id = ?1")
                .bind(self.fid)
                .fetch_optional(&pool)
                .await
                .unwrap();
        row.and_then(|r| r.0)
    }

    /// Every job row, oldest first: `(id, kind, state, resumed_from,
    /// estimated_tokens, pinned_sha)`.
    async fn jobs(&self) -> Vec<JobRow> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        sqlx::query_as(
            "SELECT id, kind, state, resumed_from, estimated_tokens, pinned_sha \
             FROM jobs ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    }
}

type JobRow = (
    i64,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

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
                severity: hunter::domain::Severity::Medium,
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
    // The harvest a withdrawal continues into; these two slots are the
    // ones the handoff tests assert on.
    std::fs::write(
        playbooks.join("harvest-closed.md"),
        "closed: the worktree is {{WORKTREE_STATE}}.\n```diff\n{{PR_DIFF}}\n```\n",
    )
    .unwrap();
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

/// On a public repo anyone can comment on hunter's PRs, and engage acts on
/// a comment by committing and pushing. So only a comment by someone who
/// can push, or by a configured review bot, flags the PR for engage; a
/// stranger's leaves it alone.
#[tokio::test]
async fn only_maintainers_and_configured_bots_flag_new_comments() {
    for (author, bots, flagged) in [
        ("stranger", vec![], false),
        ("maintainer", vec![], true),
        ("coderabbitai", vec!["coderabbitai"], true),
    ] {
        let mut f = fixture(&format!("sync-screen-{author}")).await;
        f.cfg.review_bots = hunter::forge::ReviewBots::new(bots);
        let bins = FakeBins::acquire(&format!("sync-screen-{author}"));
        bins.script(
            "gh",
            &format!(
                "case \"$*\" in\n\
                 \x20 *users/coderabbitai\\ *) echo Organization; exit 0 ;;\n\
                 \x20 *collaborators/maintainer/permission*) echo true; exit 0 ;;\n\
                 \x20 *permission*) echo false; exit 0 ;;\n\
                 esac\n\
                 cat <<'__PR_EOF__'\n\
                 {{\"state\":\"OPEN\",\"mergeable\":\"MERGEABLE\",\"reviewDecision\":\"\",\"statusCheckRollup\":[],\
                 \"updatedAt\":\"2026-01-02T03:04:05Z\",\"headRefName\":\"{BRANCH}\",\"headRefOid\":\"deadbeef\",\
                 \"reviews\":[],\"comments\":[{{\"author\":{{\"login\":\"{author}\"}},\
                 \"body\":\"please change this\",\"createdAt\":\"2026-01-02T03:04:05Z\"}}]}}\n\
                 __PR_EOF__\nexit 0"
            ),
        );

        let result = hunter::scheduler::sync_prs(&f.store, &f.cfg).await;

        assert_eq!(result.errors, 0, "{author}: {result:?}");
        assert_eq!(
            f.attention().await.as_deref(),
            flagged.then_some("new_comments"),
            "{author}"
        );
    }
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

/// A clone whose origin is the repo's URL is not refused because a
/// `url.<base>.insteadOf` rule rewrites that URL.
///
/// `git clone -- <url>` stores the URL as given, but `git remote get-url`
/// prints it after insteadOf rewriting, so a rule in the daemon user's
/// gitconfig (`url."git@github.com:".insteadOf https://github.com/` is
/// the common one) made the clone the daemon itself had just made look
/// like somebody else's: every later job for the repo was refused. The
/// rule sits in the clone's own config here so the test touches no global
/// git config; git applies it identically.
#[tokio::test]
async fn an_insteadof_rewrite_of_the_origin_is_not_refused() {
    let dir = TempDir::new("insteadof-origin");
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (_db, store) = fresh_store(&dir, "insteadof").await;

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
    // Origin is the registered URL, exactly as `git clone -- <url>`
    // records it; the rule resolves it to the local bare origin.
    support::git(&repo_dir, &["remote", "set-url", "origin", REPO_URL]);
    support::git(
        &repo_dir,
        &[
            "config",
            &format!("url.{}.insteadOf", repo.origin.display()),
            REPO_URL,
        ],
    );

    let row = store.get_repo_by_id(rid).await.unwrap().unwrap();
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("hunt.md"), "hunt {{WORKTREE}}\n").unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    let backend = ScriptedBackend::writing("findings.json", "[]");
    let summary = hunter::scheduler::run_hunt(&store, &cfg, &row, &backend, None)
        .await
        .expect("the daemon's own clone must not be refused over an insteadOf rewrite");
    assert!(
        summary.job_id.is_some(),
        "the hunt must get past the sync and run: {summary:?}"
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

// -- a withdrawal continues into its harvest ---------------------------------

/// `gh` for a withdrawal and the harvest after it: the view for every
/// call but `pr diff`.
fn gh_for_handoff(bins: &FakeBins) {
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = diff ]; then\ncat <<'__FAKE_EOF__'\n{PR_DIFF}__FAKE_EOF__\nexit 0\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
}

/// A worker that withdraws on its first run, leaving a transcript in the
/// chain's session directory (unless `keep_transcript` is false, which is
/// how a transcript goes missing between the two runs), and classifies
/// the closure on a run that continues a transcript. A transcript that is
/// not on disk is refused the way the harness refuses it. The withdrawing
/// run also leaves a `FOLLOW-UPS.json`, as engage workers did before the
/// harvest took follow-ups over; nobody verified it, so nobody may file it.
fn withdraw_then_classify(keep_transcript: bool) -> ScriptedBackend {
    ScriptedBackend::staged(move |tree, resume_from| {
        let session = tree.parent().unwrap().join("session").join("engage.jsonl");
        match resume_from {
            None => {
                std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
                std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
                if keep_transcript {
                    std::fs::write(&session, format!("{USAGE}\n")).unwrap();
                }
                hunter::types::RunResult {
                    session_file: Some(session.to_string_lossy().into_owned()),
                    ..support::done()
                }
            }
            Some(file) if !file.exists() => hunter::types::RunResult {
                exit_code: None,
                killed_reason: Some("resume-unavailable".to_owned()),
                tokens_new: 0,
                calls: 0,
                session_file: None,
                duration_s: 0.0,
                stdout_tail: "cannot resume".to_owned(),
                usage_delta: None,
            },
            Some(file) => {
                std::fs::write(
                    tree.join("CLOSE-REASON.json"),
                    r#"{"classification":"superseded","reason":"landed in #9","evidence":"abc1234"}"#,
                )
                .unwrap();
                hunter::types::RunResult {
                    session_file: Some(file.to_string_lossy().into_owned()),
                    ..support::done()
                }
            }
        }
    })
}

/// What a failed handoff must leave behind: exactly what the withdrawal
/// left. The finding `closed`, the PR unharvested with no failure counted,
/// the chain's tree released, and the harvest tier about to review it
/// cold.
async fn assert_left_for_the_cold_harvest(f: &Fixture, tree: &std::path::Path) {
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert_eq!(
        ps.harvest_attempts, 0,
        "a failed handoff is not a harvest failure"
    );
    assert!(!tree.exists(), "the chain's tree must be released");
    match hunter::scheduler::pick_next(&f.store, &f.cfg, None)
        .await
        .unwrap()
    {
        Some(hunter::scheduler::Candidate::Finding {
            kind: hunter::domain::FindingJobKind::Harvest,
            finding_id,
            ..
        }) => assert_eq!(finding_id, f.fid),
        other => panic!("expected the cold harvest next, got {other:?}"),
    }
}

/// A withdrawing engage continues straight into the closed PR's harvest,
/// as the next attempt of its own chain: resumed from the engage job, in
/// the engage's tree, continuing the engage's transcript, told the tree is
/// the PR's head, reserving what a resume of that transcript reserves.
/// The harvest's classification lands, and the tree is released after.
#[tokio::test]
async fn a_withdrawal_continues_into_its_harvest_in_the_same_session() {
    let f = fixture("handoff-ok").await;
    let bins = FakeBins::acquire("handoff-ok");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(
        runs[1].tree, runs[0].tree,
        "the harvest runs in the engage's tree"
    );
    let transcript = runs[0]
        .tree
        .parent()
        .unwrap()
        .join("session")
        .join("engage.jsonl");
    assert_eq!(runs[1].resume_from.as_deref(), Some(transcript.as_path()));
    assert!(
        runs[1]
            .prompt
            .contains("checked out at the PR's head branch; main is origin/main"),
        "{}",
        runs[1].prompt
    );
    assert!(runs[1].prompt.contains(PR_DIFF), "{}", runs[1].prompt);

    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    let (engage, harvest) = (&jobs[0], &jobs[1]);
    assert_eq!((engage.0, engage.1.as_str()), (engage_job, "engage"));
    assert_eq!(harvest.1, "harvest");
    assert_eq!(harvest.2, "done");
    assert_eq!(harvest.3, Some(engage_job), "resumed from the engage job");
    assert_eq!(
        harvest.4,
        Some(200_000),
        "the resume reservation of the transcript"
    );
    assert_eq!(harvest.5, engage.5, "the chain's pinned commit");

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Superseded);
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some("superseded: landed in #9 (evidence: abc1234)")
    );
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(ps.harvested_at.is_some());
    let all = f
        .store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        1,
        "the engage's leftover FOLLOW-UPS.json must not be filed as the harvest's: {all:?}"
    );
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");
}

/// A window with room for the engage but not for continuing its
/// transcript into the harvest: nothing else happens. No harvest job, the
/// finding stays `closed` for the cold harvest, and the tree is released.
#[tokio::test]
async fn a_denied_handoff_leaves_the_finding_closed_and_releases_the_tree() {
    let f = fixture("handoff-denied").await;
    let bins = FakeBins::acquire("handoff-denied");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // Engage reserves its cold 40,000; the handoff reserves 200,000.
    let backend = withdraw_then_classify(true).denying_above(150_000);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.1 == "engage"),
        "a denied handoff writes no job: {jobs:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The engage's transcript is gone by the time the harvest would continue
/// it: the harness refuses (`resume-unavailable`), and nothing else
/// happens either — the engage job is not rewritten, no failure counts,
/// and the finding waits `closed` for the cold harvest.
#[tokio::test]
async fn an_unavailable_transcript_leaves_the_finding_closed_and_releases_the_tree() {
    let f = fixture("handoff-unavailable").await;
    let bins = FakeBins::acquire("handoff-unavailable");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(false);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_eq!(jobs[0].2, "done", "the finished engage must stay done");
    assert_eq!(
        (jobs[1].1.as_str(), jobs[1].2.as_str()),
        ("harvest", "failed")
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The closed PR's diff will not load for the handoff. That fails the
/// handoff before any harvest job exists, but it is still no harvest
/// failure: the cold harvest keeps its full budget of attempts, so no
/// attempt may be counted toward its give-up streak.
#[tokio::test]
async fn a_handoff_whose_diff_fails_counts_no_harvest_attempt() {
    let f = fixture("handoff-diff-fails").await;
    let bins = FakeBins::acquire("handoff-diff-fails");
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = diff ]; then\necho 'HTTP 502' >&2\nexit 1\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(
        runs.len(),
        1,
        "no harvest worker without the diff: {runs:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The forge accepted the close and `pr_state` records it, but the
/// finding could not be set `closed`. A harvest run now would find the
/// finding `pr_open`, keep that as if a human had set it, and still mark
/// the PR harvested, so the `closed` the next sync writes would never be
/// harvested. Nothing continues: the finding stays `pr_open`, and the
/// next sync -- which revisits `pr_open` findings and sees the PR closed
/// -- sets it `closed` and queues the cold harvest.
///
/// The failing write is injected with a trigger that refuses to set a
/// finding's status to `closed`.
#[tokio::test]
async fn an_unrecorded_closed_verdict_starts_no_harvest_and_waits_for_the_sync() {
    let f = fixture("handoff-verdict-unrecorded").await;
    let bins = FakeBins::acquire("handoff-verdict-unrecorded");
    gh_for_handoff(&bins);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_closed_verdict BEFORE UPDATE OF status ON findings \
         WHEN NEW.status = 'closed' \
         BEGIN SELECT RAISE(ABORT, 'injected closed verdict failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.3 != Some(engage_job)),
        "no harvest may continue the engage: {jobs:?}"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");

    // The store recovers; the next sync finds the PR closed on the forge.
    sqlx::query("DROP TRIGGER no_closed_verdict")
        .execute(&pool)
        .await
        .unwrap();
    bins.ok("gh", PR_CLOSED_JSON);
    let result = hunter::scheduler::sync_prs(&f.store, &f.cfg).await;
    assert_eq!(result.closed, 1, "{result:?}");
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The same for the closed PR's view, which the harvest fetches before
/// the diff. The engage's own view (the first) succeeds; the handoff's
/// (the second) fails.
#[tokio::test]
async fn a_handoff_whose_view_fails_counts_no_harvest_attempt() {
    let f = fixture("handoff-view-fails").await;
    let bins = FakeBins::acquire("handoff-view-fails");
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = view ]; then\nviews=\"$(dirname \"$0\")/views\"\necho x >> \"$views\"\nif [ \"$(wc -l < \"$views\")\" -gt 1 ]; then\necho 'HTTP 502' >&2\nexit 1\nfi\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(
        runs.len(),
        1,
        "no harvest worker without the view: {runs:?}"
    );
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("PR/MR view failed")),
        "the handoff reached the harvest's view: {events:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The forge accepted the close but `pr_state` could not record it. The
/// harvest reads its playbook off `pr_state`, so continuing into it now
/// would review the closed PR as a merged one and mark it harvested with
/// no closure classification. Nothing continues: the finding stays
/// `pr_open`, the tree is released, and the next sync -- which sees the PR
/// closed on the forge -- records both and queues the cold harvest.
///
/// The failing write is injected with a trigger that refuses to flip a
/// `pr_state` row to CLOSED, the same way `store_write_test` breaks
/// one statement of a sync.
#[tokio::test]
async fn an_unrecorded_close_starts_no_harvest_and_waits_for_the_sync() {
    let f = fixture("handoff-unrecorded").await;
    let bins = FakeBins::acquire("handoff-unrecorded");
    gh_for_handoff(&bins);
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
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.3 != Some(engage_job)),
        "no harvest may continue the engage: {jobs:?}"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_ne!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");

    // The store recovers; the next sync finds the PR closed on the forge.
    sqlx::query("DROP TRIGGER no_close")
        .execute(&pool)
        .await
        .unwrap();
    bins.ok("gh", PR_CLOSED_JSON);
    let result = hunter::scheduler::sync_prs(&f.store, &f.cfg).await;
    assert_eq!(result.closed, 1, "{result:?}");
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}
