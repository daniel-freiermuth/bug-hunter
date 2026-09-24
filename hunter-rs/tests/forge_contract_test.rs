#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The `Forge` subprocess contract: the argv each method builds, and what
//! it does with the child's exit code.
//!
//! `forge_test.rs` covers the pure URL functions. Everything here needs a
//! process, so it needs [`FakeBins`] — which also means these tests
//! exercise the real argv construction rather than a mock of it.
//!
//! The invariant these exist for: **`close_pr` posts the withdrawal reason
//! and only closes once the comment lands.** Discarding the comment result
//! and closing regardless would turn a forge outage into a silently
//! unexplained closed PR. Two neighbouring input classes are pinned with
//! it: an *empty* reason (skipped, so a comment failure cannot block the
//! close) and a *multibyte* reason (truncated by characters — truncating
//! by bytes panics mid-codepoint).

mod support;

use hunter::domain::ForgeName;
use hunter::forge::{Mergeable, PrState, ReviewDecision, forge_for};
use support::{FakeBins, TempDir};

const GH_REPO: &str = "https://github.com/acme/widget";
const GL_REPO: &str = "https://gitlab.com/group/widget";
const GL_NESTED: &str = "https://gitlab.com/group/sub/widget";
const GL_SELF_HOSTED: &str = "https://git.example.com/team/proj";
const PR: i64 = 7;

/// Three bytes per character, so a byte-indexed truncation at 800 lands
/// mid-codepoint (800 = 266 × 3 + 2) and panics.
const WIDE: &str = "あ";

const GH_VIEW_JSON: &str = r#"{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"APPROVED","headRefName":"fix/x","headRefOid":"cafe","updatedAt":"2026-01-02T03:04:05Z","title":"t","body":"b","comments":[],"reviews":[],"statusCheckRollup":[]}"#;

const GL_MR_JSON: &str = r#"{"state":"opened","detailed_merge_status":"mergeable","source_branch":"fix/x","sha":"cafe","updated_at":"2026-01-02T03:04:05Z","title":"t","description":"d","head_pipeline":{"status":"success"}}"#;

// ---------------------------------------------------------------------------
// Reading the invocation log
// ---------------------------------------------------------------------------

/// Position of the first invocation of `bin` whose arguments begin with
/// `sub`. Positions index the whole log, so two of them compare as an
/// ordering between different binaries' calls as well.
fn position_of(calls: &[Vec<String>], bin: &str, sub: &[&str]) -> Option<usize> {
    calls.iter().position(|c| {
        c.first().is_some_and(|n| n == bin)
            && c.len() > sub.len()
            && c[1..=sub.len()].iter().zip(sub).all(|(a, b)| a == b)
    })
}

/// Position of the first invocation of `bin` carrying an argument that
/// contains `needle`. `glab api` puts the whole REST path in one argument,
/// so there is no fixed subcommand position to match on.
fn position_containing(calls: &[Vec<String>], bin: &str, needle: &str) -> Option<usize> {
    calls.iter().position(|c| {
        c.first().is_some_and(|n| n == bin) && c.iter().skip(1).any(|a| a.contains(needle))
    })
}

/// The argument following `flag`.
fn value_after<'a>(call: &'a [String], flag: &str) -> Option<&'a str> {
    let i = call.iter().position(|a| a == flag)?;
    call.get(i + 1).map(String::as_str)
}

// ---------------------------------------------------------------------------
// close_pr — GitHub
// ---------------------------------------------------------------------------

/// The reason is posted, and it is posted *before* the close. A close that
/// races ahead of its own explanation is the same user-visible failure as
/// one that never explains itself.
#[test]
fn github_close_posts_the_reason_then_closes() {
    let bins = FakeBins::acquire("forge-gh-close-ok");
    bins.ok("gh", "");

    forge_for(ForgeName::Github)
        .close_pr(GH_REPO, PR, "withdrawing: superseded upstream")
        .expect("close should succeed when every gh call does");

    let calls = bins.calls();
    let comment = position_of(&calls, "gh", &["pr", "comment"])
        .unwrap_or_else(|| panic!("no `gh pr comment` in {calls:?}"));
    let close = position_of(&calls, "gh", &["pr", "close"])
        .unwrap_or_else(|| panic!("no `gh pr close` in {calls:?}"));
    assert!(
        comment < close,
        "the reason must land before the close: {calls:?}"
    );

    assert_eq!(
        calls[comment],
        [
            "gh",
            "pr",
            "comment",
            "7",
            "-R",
            "acme/widget",
            "--body",
            "withdrawing: superseded upstream"
        ]
    );
    assert_eq!(
        calls[close],
        ["gh", "pr", "close", "7", "-R", "acme/widget"]
    );
}

/// The regression itself: a forge that rejects the comment must abort the
/// whole withdrawal, not close the PR with the reason thrown away.
#[test]
fn github_close_is_not_issued_when_the_comment_fails() {
    let bins = FakeBins::acquire("forge-gh-close-comment-fails");
    bins.ok_unless_action("gh", "pr comment", "");

    let err = forge_for(ForgeName::Github)
        .close_pr(GH_REPO, PR, "withdrawing: superseded upstream")
        .expect_err("a rejected comment must fail the close");
    assert!(
        err.to_string().contains("comment"),
        "the error must name the step that failed, got: {err}"
    );

    let calls = bins.calls();
    assert!(
        position_of(&calls, "gh", &["pr", "close"]).is_none(),
        "close must not be issued once the reason was rejected: {calls:?}"
    );
}

/// An empty reason is skipped rather than posted. Pinned with `gh pr
/// comment` scripted to fail: if the skip ever goes away, posting an empty
/// body would surface as an `Err` and an un-closed PR.
#[test]
fn github_close_skips_an_empty_comment_and_still_closes() {
    let bins = FakeBins::acquire("forge-gh-close-empty");
    bins.ok_unless_action("gh", "pr comment", "");

    forge_for(ForgeName::Github)
        .close_pr(GH_REPO, PR, "")
        .expect("an empty reason must not be posted, so nothing can reject it");

    let calls = bins.calls();
    assert!(
        position_of(&calls, "gh", &["pr", "comment"]).is_none(),
        "no comment call may be made for an empty reason: {calls:?}"
    );
    assert!(
        position_of(&calls, "gh", &["pr", "close"]).is_some(),
        "the close must still be issued: {calls:?}"
    );
}

/// The skip is `is_empty()`, not "blank". Whitespace is a real comment and
/// is posted verbatim — pinned so the emptiness test above is known to be
/// asserting on a narrow boundary rather than on any falsy-looking string.
#[test]
fn github_close_treats_whitespace_as_a_real_comment() {
    let bins = FakeBins::acquire("forge-gh-close-blank");
    bins.ok("gh", "");

    forge_for(ForgeName::Github)
        .close_pr(GH_REPO, PR, "   ")
        .unwrap();

    let calls = bins.calls();
    let comment = position_of(&calls, "gh", &["pr", "comment"])
        .unwrap_or_else(|| panic!("whitespace is not empty and must be posted: {calls:?}"));
    assert_eq!(value_after(&calls[comment], "--body"), Some("   "));
}

/// 800 *characters*, not 800 bytes. Slicing a multibyte reason at byte 800
/// panics mid-codepoint, which took down the whole withdrawal path.
#[test]
fn github_close_truncates_the_comment_to_800_chars_not_bytes() {
    let bins = FakeBins::acquire("forge-gh-close-wide");
    bins.ok("gh", "");

    let reason = WIDE.repeat(1000);
    assert_eq!(reason.len(), 3000, "fixture must be multibyte");

    forge_for(ForgeName::Github)
        .close_pr(GH_REPO, PR, &reason)
        .expect("a multibyte reason must not panic or fail");

    let calls = bins.calls();
    let comment = position_of(&calls, "gh", &["pr", "comment"]).expect("comment call");
    let posted = value_after(&calls[comment], "--body").expect("--body value");
    assert_eq!(posted.chars().count(), 800, "truncated by characters");
    assert_eq!(posted.len(), 2400, "which is 2400 bytes, not 800");
    assert_eq!(posted, WIDE.repeat(800), "the leading 800 characters");
    assert!(
        position_of(&calls, "gh", &["pr", "close"]).is_some(),
        "and the close still happens: {calls:?}"
    );
}

// ---------------------------------------------------------------------------
// close_pr — GitLab
// ---------------------------------------------------------------------------

/// Same contract, different CLI: the note goes through `glab api`, the
/// close through `glab mr close`, and the note comes first.
#[test]
fn gitlab_close_posts_the_reason_then_closes() {
    let bins = FakeBins::acquire("forge-gl-close-ok");
    bins.ok("glab", "");

    forge_for(ForgeName::Gitlab)
        .close_pr(GL_REPO, PR, "withdrawing: superseded upstream")
        .expect("close should succeed when every glab call does");

    let calls = bins.calls();
    let note = position_containing(&calls, "glab", "/notes")
        .unwrap_or_else(|| panic!("no notes POST in {calls:?}"));
    let close = position_of(&calls, "glab", &["mr", "close"])
        .unwrap_or_else(|| panic!("no `glab mr close` in {calls:?}"));
    assert!(
        note < close,
        "the reason must land before the close: {calls:?}"
    );

    assert_eq!(
        calls[note],
        [
            "glab",
            "api",
            "projects/group%2Fwidget/merge_requests/7/notes",
            "--method",
            "POST",
            "-f",
            "body=withdrawing: superseded upstream",
        ]
    );
    assert_eq!(
        calls[close],
        ["glab", "mr", "close", "7", "-R", "group/widget"]
    );
}

#[test]
fn gitlab_close_is_not_issued_when_the_comment_fails() {
    let bins = FakeBins::acquire("forge-gl-close-comment-fails");
    // Every `glab api` fails; `glab mr close` would still succeed.
    bins.ok_unless_action("glab", "api", "");

    let err = forge_for(ForgeName::Gitlab)
        .close_pr(GL_REPO, PR, "withdrawing: superseded upstream")
        .expect_err("a rejected note must fail the close");
    assert!(
        err.to_string().contains("notes"),
        "the error must name the step that failed, got: {err}"
    );

    let calls = bins.calls();
    assert!(
        position_of(&calls, "glab", &["mr", "close"]).is_none(),
        "close must not be issued once the reason was rejected: {calls:?}"
    );
}

#[test]
fn gitlab_close_skips_an_empty_comment_and_still_closes() {
    let bins = FakeBins::acquire("forge-gl-close-empty");
    bins.ok_unless_action("glab", "api", "");

    forge_for(ForgeName::Gitlab)
        .close_pr(GL_REPO, PR, "")
        .expect("an empty reason must not be posted, so nothing can reject it");

    let calls = bins.calls();
    assert!(
        position_containing(&calls, "glab", "/notes").is_none(),
        "no notes POST may be made for an empty reason: {calls:?}"
    );
    assert!(
        position_of(&calls, "glab", &["mr", "close"]).is_some(),
        "the close must still be issued: {calls:?}"
    );
}

#[test]
fn gitlab_close_truncates_the_comment_to_800_chars_not_bytes() {
    let bins = FakeBins::acquire("forge-gl-close-wide");
    bins.ok("glab", "");

    let reason = WIDE.repeat(1000);

    forge_for(ForgeName::Gitlab)
        .close_pr(GL_REPO, PR, &reason)
        .expect("a multibyte reason must not panic or fail");

    let calls = bins.calls();
    let note = position_containing(&calls, "glab", "/notes").expect("notes POST");
    let field = value_after(&calls[note], "-f").expect("-f value");
    let posted = field
        .strip_prefix("body=")
        .unwrap_or_else(|| panic!("note body must be sent as `body=`, got {field:?}"));
    assert_eq!(posted.chars().count(), 800, "truncated by characters");
    assert_eq!(posted.len(), 2400, "which is 2400 bytes, not 800");
    assert_eq!(posted, WIDE.repeat(800), "the leading 800 characters");
    assert!(
        position_of(&calls, "glab", &["mr", "close"]).is_some(),
        "and the close still happens: {calls:?}"
    );
}

/// Self-hosted GitLab needs `--hostname` on the API call and a fully
/// qualified `-R` on the close; gitlab.com needs neither. Getting this
/// wrong sends the withdrawal to the wrong instance.
#[test]
fn gitlab_close_targets_a_self_hosted_host() {
    let bins = FakeBins::acquire("forge-gl-close-selfhosted");
    bins.ok("glab", "");

    forge_for(ForgeName::Gitlab)
        .close_pr(GL_SELF_HOSTED, PR, "withdrawing")
        .unwrap();

    let calls = bins.calls();
    let note = position_containing(&calls, "glab", "/notes").expect("notes POST");
    assert_eq!(
        calls[note],
        [
            "glab",
            "api",
            "projects/team%2Fproj/merge_requests/7/notes",
            "--method",
            "POST",
            "-f",
            "body=withdrawing",
            "--hostname",
            "git.example.com",
        ]
    );
    let close = position_of(&calls, "glab", &["mr", "close"]).expect("close");
    assert_eq!(
        calls[close],
        [
            "glab",
            "mr",
            "close",
            "7",
            "-R",
            "https://git.example.com/team/proj",
        ]
    );
}

// ---------------------------------------------------------------------------
// create_pr
// ---------------------------------------------------------------------------

/// The URL comes from the last line of `gh`'s chatter, and an empty title
/// falls back to the branch name — `gh pr create --title ""` is rejected
/// by the CLI, so the fallback is load-bearing.
#[test]
fn github_create_pr_returns_the_url_and_defaults_the_title_to_the_branch() {
    let bins = FakeBins::acquire("forge-gh-create-ok");
    let dir = TempDir::new("forge-gh-create-ok-cwd");
    bins.ok(
        "gh",
        "Creating draft pull request for fix/x into main\nhttps://github.com/acme/widget/pull/9",
    );

    let url = forge_for(ForgeName::Github)
        .create_pr(dir.path(), "fix/x", "main", "", "body text")
        .expect("create should succeed on rc=0");
    assert_eq!(url, "https://github.com/acme/widget/pull/9");

    let calls = bins.calls();
    let create = position_of(&calls, "gh", &["pr", "create"]).expect("create call");
    assert_eq!(
        calls[create],
        [
            "gh",
            "pr",
            "create",
            "--draft",
            "--head",
            "fix/x",
            "--base",
            "main",
            "--title",
            "fix/x",
            "--body",
            "body text",
        ],
        "an empty title must fall back to the branch name"
    );
}

#[test]
fn github_create_pr_propagates_a_non_zero_exit() {
    let bins = FakeBins::acquire("forge-gh-create-fail");
    let dir = TempDir::new("forge-gh-create-fail-cwd");
    bins.fail("gh", 3, "gh: not authenticated");

    let err = forge_for(ForgeName::Github)
        .create_pr(dir.path(), "fix/x", "main", "t", "b")
        .expect_err("a non-zero gh must not yield a PR URL");
    let msg = err.to_string();
    assert!(msg.contains("rc=3"), "exit code must survive: {msg}");
    assert!(
        msg.contains("not authenticated"),
        "the CLI's diagnostics must survive: {msg}"
    );
}

/// `glab` prints the MR URL and then keeps talking, so the URL is found by
/// searching backwards for the merge-request path — not by taking the last
/// line, which would return the trailing hint.
#[test]
fn gitlab_create_pr_finds_the_url_even_with_trailing_output() {
    let bins = FakeBins::acquire("forge-gl-create-ok");
    let dir = TempDir::new("forge-gl-create-ok-cwd");
    bins.ok(
        "glab",
        "Creating merge request for fix/x into main\nhttps://gitlab.com/group/widget/-/merge_requests/3\nView this merge request with: glab mr view 3",
    );

    let url = forge_for(ForgeName::Gitlab)
        .create_pr(dir.path(), "fix/x", "main", "", "body text")
        .expect("create should succeed on rc=0");
    assert_eq!(url, "https://gitlab.com/group/widget/-/merge_requests/3");

    let calls = bins.calls();
    let create = position_of(&calls, "glab", &["mr", "create"]).expect("create call");
    assert_eq!(
        calls[create],
        [
            "glab",
            "mr",
            "create",
            "--source-branch",
            "fix/x",
            "--target-branch",
            "main",
            "--draft",
            "--title",
            "fix/x",
            "--description",
            "body text",
            "--yes",
        ],
        "an empty title must fall back to the branch name"
    );
}

#[test]
fn gitlab_create_pr_propagates_a_non_zero_exit() {
    let bins = FakeBins::acquire("forge-gl-create-fail");
    let dir = TempDir::new("forge-gl-create-fail-cwd");
    bins.fail("glab", 4, "glab: 403 forbidden");

    let err = forge_for(ForgeName::Gitlab)
        .create_pr(dir.path(), "fix/x", "main", "t", "b")
        .expect_err("a non-zero glab must not yield an MR URL");
    let msg = err.to_string();
    assert!(msg.contains("rc=4"), "exit code must survive: {msg}");
    assert!(
        msg.contains("403 forbidden"),
        "the CLI's diagnostics must survive: {msg}"
    );
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

/// Pushes go to a raw SSH URL, never a named remote: `--force-with-lease`
/// cannot resolve a tracking ref for a bare URL, so the pair (raw URL,
/// plain `--force`) has to stay together.
///
/// `http://` remotes are rewritten too. They are accepted by
/// `valid_repo_url`, and pushing to one unrewritten would carry a force
/// push over an unauthenticated, unencrypted hop instead of SSH.
#[test]
fn push_forces_to_a_raw_ssh_url_for_both_forges() {
    let bins = FakeBins::acquire("forge-push-ok");
    let dir = TempDir::new("forge-push-ok-cwd");
    bins.ok("git", "");

    forge_for(ForgeName::Github)
        .push(dir.path(), GH_REPO, "fix/x")
        .unwrap();
    forge_for(ForgeName::Gitlab)
        .push(dir.path(), GL_REPO, "fix/x")
        .unwrap();
    forge_for(ForgeName::Github)
        .push(dir.path(), "http://github.com/acme/widget", "fix/x")
        .unwrap();
    forge_for(ForgeName::Gitlab)
        .push(dir.path(), "http://gitlab.com/group/widget", "fix/x")
        .unwrap();

    let calls = bins.calls_to("git");
    assert_eq!(
        calls,
        vec![
            vec![
                "git",
                "push",
                "--force",
                "git@github.com:acme/widget.git",
                "HEAD:fix/x",
            ],
            vec![
                "git",
                "push",
                "--force",
                "git@gitlab.com:group/widget.git",
                "HEAD:fix/x",
            ],
            vec![
                "git",
                "push",
                "--force",
                "git@github.com:acme/widget.git",
                "HEAD:fix/x",
            ],
            vec![
                "git",
                "push",
                "--force",
                "git@gitlab.com:group/widget.git",
                "HEAD:fix/x",
            ],
        ]
    );
}

#[test]
fn push_propagates_a_non_zero_git_exit() {
    let bins = FakeBins::acquire("forge-push-fail");
    let dir = TempDir::new("forge-push-fail-cwd");
    bins.fail("git", 128, "remote rejected: protected branch");

    for forge in [ForgeName::Github, ForgeName::Gitlab] {
        let err = forge_for(forge)
            .push(dir.path(), GH_REPO, "fix/x")
            .expect_err("a rejected push must not report success");
        let msg = err.to_string();
        assert!(msg.contains("rc=128"), "{forge:?}: exit code lost: {msg}");
        assert!(
            msg.contains("protected branch"),
            "{forge:?}: git's diagnostics lost: {msg}"
        );
    }
}

// ---------------------------------------------------------------------------
// view_pr_sync / view_pr_engage
// ---------------------------------------------------------------------------

/// A failed view must be an error, never a default `PrView` — a
/// default-filled struct reads as "open, no checks, no comments", which is
/// indistinguishable from a real answer and drives the wrong decision.
#[test]
fn github_view_errors_on_a_non_zero_exit() {
    let bins = FakeBins::acquire("forge-gh-view-rc");
    bins.fail("gh", 1, "could not resolve to a PullRequest");
    let gh = forge_for(ForgeName::Github);

    let err = gh
        .view_pr_sync(GH_REPO, PR)
        .expect_err("sync view must fail on rc!=0");
    assert!(err.to_string().contains("rc=1"), "got: {err}");
    assert!(
        gh.view_pr_engage(GH_REPO, PR).is_err(),
        "engage view must fail on rc!=0 too"
    );
}

#[test]
fn github_view_errors_on_unparseable_json() {
    let bins = FakeBins::acquire("forge-gh-view-garbage");
    // rc=0 with a login redirect / proxy error page in stdout.
    bins.ok("gh", "<!DOCTYPE html><html>502 Bad Gateway</html>");
    let gh = forge_for(ForgeName::Github);

    assert!(
        gh.view_pr_sync(GH_REPO, PR).is_err(),
        "unparseable output must not become a default PrView"
    );
    assert!(gh.view_pr_engage(GH_REPO, PR).is_err());
}

/// The boundary of the test above: *unparseable* is an error, *incomplete*
/// is not. Valid JSON with every field missing is accepted with defaults,
/// and states are matched case-insensitively.
#[test]
fn github_view_accepts_valid_json_with_fields_missing() {
    let bins = FakeBins::acquire("forge-gh-view-sparse");
    let gh = forge_for(ForgeName::Github);

    bins.ok("gh", "{}");
    let view = gh
        .view_pr_sync(GH_REPO, PR)
        .expect("an empty object is valid JSON");
    assert_eq!(view.state, PrState::Open, "absent state defaults to open");
    assert_eq!(view.mergeable, Mergeable::Unknown);
    assert!(view.head_ref.is_empty());
    assert!(view.comments.is_empty());

    bins.ok(
        "gh",
        r#"{"state":"merged","mergeable":"conflicting","reviewDecision":"changes_requested","statusCheckRollup":"not-an-array"}"#,
    );
    let view = gh.view_pr_sync(GH_REPO, PR).expect("lowercase is valid");
    assert_eq!(view.state, PrState::Merged);
    assert_eq!(view.mergeable, Mergeable::Conflicting);
    assert_eq!(view.review_decision, ReviewDecision::ChangesRequested);
    assert!(
        view.status_check_rollup.is_empty(),
        "a rollup of the wrong shape is dropped, not fatal"
    );
}

/// Engage needs the PR's title and body to brief the worker; sync does
/// not, and pays for the fields it asks for. The two field sets must stay
/// distinct.
#[test]
fn github_sync_and_engage_request_different_field_sets() {
    let bins = FakeBins::acquire("forge-gh-view-fields");
    bins.ok("gh", GH_VIEW_JSON);
    let gh = forge_for(ForgeName::Github);

    gh.view_pr_sync(GH_REPO, PR).unwrap();
    gh.view_pr_engage(GH_REPO, PR).unwrap();

    let calls = bins.calls_to("gh");
    assert_eq!(calls.len(), 2, "one call each: {calls:?}");
    let fields = |c: &Vec<String>| -> Vec<String> {
        assert_eq!(c[..6], ["gh", "pr", "view", "7", "-R", "acme/widget"]);
        value_after(c, "--json")
            .expect("--json value")
            .split(',')
            .map(str::to_owned)
            .collect()
    };

    let sync = fields(&calls[0]);
    let engage = fields(&calls[1]);
    for required in ["state", "mergeable", "statusCheckRollup", "updatedAt"] {
        assert!(
            sync.iter().any(|f| f == required),
            "sync must request {required}: {sync:?}"
        );
    }
    for required in ["title", "body", "comments", "reviews", "headRefOid"] {
        assert!(
            engage.iter().any(|f| f == required),
            "engage must request {required}: {engage:?}"
        );
    }
    assert!(
        !sync.iter().any(|f| f == "body"),
        "sync must not pay for the body: {sync:?}"
    );
}

#[test]
fn gitlab_view_errors_on_a_failed_mr_fetch() {
    let bins = FakeBins::acquire("forge-gl-view-rc");
    bins.fail("glab", 1, "401 Unauthorized");
    let gl = forge_for(ForgeName::Gitlab);

    let err = gl
        .view_pr_sync(GL_REPO, PR)
        .expect_err("sync view must fail on rc!=0");
    assert!(err.to_string().contains("rc=1"), "got: {err}");
    assert!(
        gl.view_pr_engage(GL_REPO, PR).is_err(),
        "engage view must fail on rc!=0 too"
    );
}

#[test]
fn gitlab_view_errors_on_unparseable_mr_json() {
    let bins = FakeBins::acquire("forge-gl-view-garbage");
    bins.ok("glab", "<!DOCTYPE html><html>502 Bad Gateway</html>");
    let gl = forge_for(ForgeName::Gitlab);

    assert!(
        gl.view_pr_sync(GL_REPO, PR).is_err(),
        "unparseable output must not become a default PrView"
    );
    assert!(gl.view_pr_engage(GL_REPO, PR).is_err());
}

/// Notes are a second request, and it is allowed to fail: the MR's own
/// state is still worth having. Pinned because the asymmetry is
/// deliberate — tightening it would make every notes hiccup look like an
/// unreachable MR.
#[test]
fn gitlab_view_survives_a_failing_notes_endpoint() {
    let bins = FakeBins::acquire("forge-gl-view-notes-down");
    bins.script(
        "glab",
        &format!(
            "case \"$*\" in\n  *notes*) echo 'notes endpoint is down' >&2; exit 1 ;;\nesac\ncat <<'__MR_EOF__'\n{GL_MR_JSON}\n__MR_EOF__\nexit 0"
        ),
    );

    let view = forge_for(ForgeName::Gitlab)
        .view_pr_sync(GL_REPO, PR)
        .expect("a failed notes fetch must not fail the whole view");

    assert_eq!(view.state, PrState::Open);
    assert_eq!(view.mergeable, Mergeable::Mergeable);
    assert_eq!(view.head_ref, "fix/x");
    assert_eq!(view.head_sha, "cafe");
    assert!(view.comments.is_empty(), "no notes could be fetched");
    assert!(view.reviews.is_empty());
    assert_eq!(
        view.status_check_rollup.len(),
        1,
        "the pipeline still normalises"
    );
    assert_eq!(bins.calls_to("glab").len(), 2, "both endpoints were tried");
}

/// GitLab project paths are nested and go into the URL path, so every `/`
/// must be percent-encoded. An unencoded slash silently addresses a
/// different (usually nonexistent) project.
#[test]
fn gitlab_view_encodes_nested_group_paths() {
    let bins = FakeBins::acquire("forge-gl-view-nested");
    bins.ok("glab", GL_MR_JSON);

    forge_for(ForgeName::Gitlab)
        .view_pr_sync(GL_NESTED, PR)
        .expect("nested group MR view");

    let calls = bins.calls_to("glab");
    assert_eq!(
        calls[0],
        [
            "glab",
            "api",
            "projects/group%2Fsub%2Fwidget/merge_requests/7",
        ]
    );
    assert_eq!(
        calls[1],
        [
            "glab",
            "api",
            "projects/group%2Fsub%2Fwidget/merge_requests/7/notes?sort=desc&per_page=100",
        ]
    );
}

/// GitLab caps `per_page` at 100 and this is a single request, so it asks
/// for the newest page — an MR whose thread is longer than that would
/// otherwise lose exactly the recent feedback `engage` exists to act on.
/// Consumers still want oldest-first, so the page is flipped back.
#[test]
fn gitlab_view_fetches_the_newest_notes_and_restores_order() {
    let bins = FakeBins::acquire("forge-gl-view-notes-order");
    // Newest-first, as `sort=desc` returns them.
    let notes = r#"[{"body":"third","created_at":"2026-01-03T00:00:00Z"},{"body":"second","created_at":"2026-01-02T00:00:00Z"},{"body":"first","created_at":"2026-01-01T00:00:00Z"}]"#;
    bins.script(
        "glab",
        &format!(
            "case \"$*\" in\n  *notes*) cat <<'__NOTES_EOF__'\n{notes}\n__NOTES_EOF__\n  exit 0 ;;\nesac\ncat <<'__MR_EOF__'\n{GL_MR_JSON}\n__MR_EOF__\nexit 0"
        ),
    );

    let view = forge_for(ForgeName::Gitlab)
        .view_pr_sync(GL_REPO, PR)
        .expect("MR view with notes");

    let bodies: Vec<&str> = view.comments.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(
        bodies,
        ["first", "second", "third"],
        "notes must reach consumers oldest-first"
    );

    let notes_call =
        position_containing(&bins.calls(), "glab", "/notes").expect("a notes request must be made");
    assert_eq!(
        bins.calls()[notes_call],
        [
            "glab",
            "api",
            "projects/group%2Fwidget/merge_requests/7/notes?sort=desc&per_page=100",
        ]
    );
}
