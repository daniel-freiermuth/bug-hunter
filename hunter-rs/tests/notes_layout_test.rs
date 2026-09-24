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
use support::TempDir;

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

    let clone_dir = Store::repo_dir(&work_root.join("repos"), 12);
    std::fs::create_dir_all(&clone_dir).unwrap();
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

    let clone_dir = Store::repo_dir(&work_root.join("repos"), 5);
    std::fs::create_dir_all(&clone_dir).unwrap();
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
