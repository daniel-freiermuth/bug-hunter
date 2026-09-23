#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Notes belong beside the clones, never inside one.
//!
//! Two separate failures came from writing them to
//! `repos/repo-<id>/NOTES.md`. The file sat untracked in the working tree
//! of a real checkout, so a worker doing a broad `git add` would commit
//! the operator's private notes into a pull request. And creating it
//! created the clone directory as a side effect, which `sync_repo` reads
//! as an existing clone and then refuses to work in, because a
//! notes-only directory has no `origin` to match -- permanently, since
//! nothing removes it.

use std::path::Path;

use hunter::store::Store;

mod support;
use support::{TempDir, git};

/// A clone directory as the daemon has one: a real git checkout.
///
/// The migration asks git whether `NOTES.md` is tracked and moves it only
/// on a definite "no", so a plain directory -- where git answers "not a
/// repository" -- is deliberately left alone and cannot stand in for a
/// clone here. `support::git` rather than a local helper: it also disables
/// commit signing and hooks, so a developer's global git config cannot
/// hang or hijack the fixture's commit.
fn clone_at(work_root: &Path, id: i64) -> std::path::PathBuf {
    let clone_dir = Store::repo_dir(&work_root.join("repos"), id);
    std::fs::create_dir_all(&clone_dir).unwrap();
    git(&clone_dir, &["init", "-q"]);
    clone_dir
}

/// Writing a note must not create the clone directory.
#[test]
fn a_note_does_not_create_the_clone_directory() {
    let dir = TempDir::new("notes-layout");
    let work_root: &Path = dir.path();

    Store::append_repo_note(work_root, 7, "widget", "first note", None).unwrap();

    let clone_dir = Store::repo_dir(&work_root.join("repos"), 7);
    assert!(
        !clone_dir.exists(),
        "{} was created by writing a note; sync_repo would then refuse to \
         clone into it for good",
        clone_dir.display()
    );

    let notes = Store::notes_path(work_root, 7);
    assert!(notes.is_file(), "{} should hold the note", notes.display());
    assert!(
        Store::repo_notes(work_root, 7).contains("first note"),
        "the note must be readable back from its new home"
    );
}

/// A note written next to a clone stays out of the checkout.
#[test]
fn notes_are_not_inside_the_checkout() {
    let dir = TempDir::new("notes-outside");
    let work_root: &Path = dir.path();

    // A clone already on disk, as a hunted repo has.
    let clone_dir = Store::repo_dir(&work_root.join("repos"), 3);
    std::fs::create_dir_all(clone_dir.join(".git")).unwrap();

    Store::append_repo_note(work_root, 3, "widget", "private context", None).unwrap();

    assert!(
        !clone_dir.join("NOTES.md").exists(),
        "a note must never land inside the checkout, where `git add -A` \
         would commit it into a pull request"
    );
    assert!(Store::notes_path(work_root, 3).is_file());
}

/// An installation that wrote notes under the old layout keeps them.
#[test]
fn startup_moves_notes_out_of_the_old_layout() {
    let dir = TempDir::new("notes-migrate");
    let work_root: &Path = dir.path();

    let clone_dir = clone_at(work_root, 12);
    std::fs::write(
        clone_dir.join("NOTES.md"),
        "# Notes: widget\n\n- old entry\n",
    )
    .unwrap();

    let moved = hunter::server::migrate_repo_notes(work_root);

    assert_eq!(moved, 1);
    assert!(
        !clone_dir.join("NOTES.md").exists(),
        "the old file must be gone, not merely copied -- otherwise it is \
         still sitting in the checkout waiting to be committed"
    );
    assert!(
        Store::repo_notes(work_root, 12).contains("old entry"),
        "the note's content must survive the move"
    );

    // Idempotent: a second pass has nothing to do.
    assert_eq!(hunter::server::migrate_repo_notes(work_root), 0);
}

/// The migration never overwrites a newer note.
#[test]
fn migration_keeps_the_current_notes() {
    let dir = TempDir::new("notes-migrate-clash");
    let work_root: &Path = dir.path();

    let clone_dir = clone_at(work_root, 5);
    std::fs::write(clone_dir.join("NOTES.md"), "stale copy\n").unwrap();
    Store::append_repo_note(work_root, 5, "widget", "current note", None).unwrap();

    hunter::server::migrate_repo_notes(work_root);

    let notes = Store::repo_notes(work_root, 5);
    assert!(
        notes.contains("current note") && !notes.contains("stale copy"),
        "a note already at the new path is the live one: {notes}"
    );
    // And the copy still leaves the checkout. Skipping the entry left it
    // in the working tree for good, which is the hazard the move exists
    // to remove -- a worker's broad `git add` can still commit it.
    assert!(
        !clone_dir.join("NOTES.md").exists(),
        "the legacy file must not stay in the clone"
    );
    assert!(
        std::fs::read_to_string(work_root.join("notes").join("repo-5.legacy.md"))
            .unwrap()
            .contains("stale copy"),
        "the operator's older writing is parked, not deleted"
    );
}

/// A `NOTES.md` the project itself tracks is the project's, not hunter's.
///
/// `NOTES.md` is a common name. Moving a tracked one out of the clone would
/// leave a deletion in the working tree for a worker's `git add -A` to
/// commit into a pull request, and serve the project's own document on the
/// Repos page as if the operator had written it.
#[test]
fn a_tracked_notes_file_is_left_in_the_project() {
    let dir = TempDir::new("notes-tracked");
    let work_root: &Path = dir.path();

    let clone_dir = clone_at(work_root, 4);
    std::fs::write(clone_dir.join("NOTES.md"), "# Project design notes\n").unwrap();
    git(&clone_dir, &["add", "NOTES.md"]);
    git(&clone_dir, &["commit", "-q", "-m", "docs"]);

    assert_eq!(hunter::server::migrate_repo_notes(work_root), 0);

    assert_eq!(
        std::fs::read_to_string(clone_dir.join("NOTES.md")).unwrap(),
        "# Project design notes\n",
        "the project's file must be left exactly as it was"
    );
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&clone_dir)
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "the clone must stay clean, got: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    assert!(
        !Store::notes_path(work_root, 4).exists(),
        "the project's document must not become the operator's notes"
    );
}

/// When git cannot say whether the file is tracked, it stays put.
///
/// Leaving a file where it is can be corrected later; moving a project's
/// file out of its checkout cannot be undone by the next pass.
#[test]
fn a_notes_file_git_cannot_classify_is_left_alone() {
    let dir = TempDir::new("notes-unknown");
    let work_root: &Path = dir.path();

    // Not a repository: `git ls-files` exits 128.
    let clone_dir = Store::repo_dir(&work_root.join("repos"), 6);
    std::fs::create_dir_all(&clone_dir).unwrap();
    std::fs::write(clone_dir.join("NOTES.md"), "unknown\n").unwrap();

    assert_eq!(hunter::server::migrate_repo_notes(work_root), 0);
    assert!(clone_dir.join("NOTES.md").is_file());
    assert!(!Store::notes_path(work_root, 6).exists());
}
