-- no-transaction
-- Stop reusing repo ids.
--
-- `id INTEGER PRIMARY KEY` is a rowid alias, so SQLite hands the highest
-- freed number to the next INSERT. Every hazard this branch has chased
-- around the repo lifecycle descends from that one fact: a replacement
-- repo inheriting a deleted repo's clone directory, its notes, its
-- cached notes in the browser, and -- the one that cannot be fixed by
-- ordering the cleanup -- a client that still holds the old id issuing a
-- pause, a delete or a note against whatever repo now answers to it.
--
-- The cleanup ordering (migration 008's two phases) closes the window
-- where files outlive the row. It cannot close the window where a
-- *client* outlives the row, because that window is however long a
-- browser tab stays open. Binding every mutation to a composite identity
-- would be the alternative, and it would put the burden on every current
-- and future endpoint.
--
-- AUTOINCREMENT removes the premise instead: ids come from
-- sqlite_sequence, which only ever moves forward, so a freed id is never
-- handed out again and a stale reference can only ever miss.
--
-- The cost is honest and small: one extra sqlite_sequence row and its
-- update per insert, on a table that takes a handful of rows a month,
-- and a hard ceiling at 2^63-1 ids -- reachable in roughly 300 billion
-- years at this repo's insert rate.
--
-- Two-phase deletion stays. It is still what stops a half-removed
-- directory being treated as a clone, and the reaper is still what
-- retries a reclamation that failed.
--
-- Rebuild, because AUTOINCREMENT cannot be added by ALTER TABLE.
--
-- Build-copy-drop-rename, NOT migration 003's rename-first shape. 003
-- renames the old table out of the way first, which is safe only because
-- nothing references `findings`. `repos` is referenced by findings and
-- jobs, and `ALTER TABLE repos RENAME TO _repos_old` rewrites those FK
-- clauses to point at `_repos_old` -- after which the old table is
-- dropped and every insert into findings fails with "no such table:
-- main._repos_old". `PRAGMA legacy_alter_table = ON` is the documented
-- defence and it does not help here: sqlx runs each migration inside a
-- transaction, and the pragma does not take effect there.
--
-- So the referenced name is never renamed. The new table is built under
-- a temporary name, `repos` is dropped, and the new table is renamed
-- INTO the referenced name -- a rename whose old name nothing
-- references. findings.repo_id and jobs.repo_id keep their numbers,
-- preserved verbatim by the copy.
--
-- Hence `-- no-transaction` at the top of this file. Neither in-
-- transaction option works:
--   * `PRAGMA foreign_keys = OFF` is documented as a no-op inside a
--     transaction, so the DROP fails with FOREIGN KEY constraint failed.
--   * `PRAGMA defer_foreign_keys = ON` defers the check to COMMIT, but
--     dropping a referenced table counts one violation per child row and
--     recreating the table does not clear that counter, so COMMIT fails
--     with the same error even though the data is consistent.
-- Outside a transaction the pragma behaves normally.
--
-- The cost of no-transaction is that a failure part-way leaves the
-- database between states. That is bounded here: the only destructive
-- statement is the DROP, and everything it destroys has already been
-- copied into _repos_new by the statement before it. A crash between
-- them leaves _repos_new behind and `repos` intact, and a re-run starts
-- from a CREATE that would fail on the leftover -- visible, not silent.
PRAGMA foreign_keys = OFF;

CREATE TABLE _repos_new (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  name          TEXT NOT NULL UNIQUE,
  url           TEXT NOT NULL,             -- https or ssh remote
  path          TEXT NOT NULL,             -- local clone dir (workRoot/repos/repo-<id>)
  forge          TEXT NOT NULL DEFAULT 'github', -- github | gitlab
  default_branch TEXT NOT NULL DEFAULT 'main',
  last_hunt_sha TEXT,                      -- HEAD at last completed hunt
  last_hunt_at  INTEGER,                   -- epoch ms
  enabled       INTEGER NOT NULL DEFAULT 1,
  added_at      INTEGER NOT NULL,
  last_full_hunt_at INTEGER,
  last_test_gap_at  INTEGER,
  last_dep_update_at INTEGER,
  last_refactor_at  INTEGER,
  last_modernization_at INTEGER,
  last_standards_at INTEGER,
  deleted_at    INTEGER                    -- flagged for reclamation; see soft_delete_repo
);

INSERT INTO _repos_new (
  id, name, url, path, forge, default_branch, last_hunt_sha, last_hunt_at,
  enabled, added_at, last_full_hunt_at, last_test_gap_at, last_dep_update_at,
  last_refactor_at, last_modernization_at, last_standards_at, deleted_at
)
SELECT
  id, name, url, path, forge, default_branch, last_hunt_sha, last_hunt_at,
  enabled, added_at, last_full_hunt_at, last_test_gap_at, last_dep_update_at,
  last_refactor_at, last_modernization_at, last_standards_at, deleted_at
FROM repos;

DROP TABLE repos;
ALTER TABLE _repos_new RENAME TO repos;

-- Recreate what lived on the old table: the DROP took migration 008's
-- partial index with it.
CREATE INDEX IF NOT EXISTS repos_deleted_at ON repos(deleted_at)
  WHERE deleted_at IS NOT NULL;

-- Without this, sqlite_sequence starts from the max id present, which is
-- correct today but not after the newest repo is deleted: the sequence
-- row is only written by an INSERT, so a rebuild that ends with no
-- sequence row would hand out an id that a deleted repo had used. The
-- INSERT above sets it when rows exist; this covers the empty case and
-- makes the floor explicit either way.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'repos', (SELECT COALESCE(MAX(id), 0) FROM repos)
WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'repos');
