-- Two-phase repo deletion, because the row is what reserves the id.
--
-- Deleting the row and then removing repos/repo-<id> is a race, not just
-- an ordering preference. `id` is a rowid alias, so the moment the row is
-- gone SQLite may hand that number to the next INSERT -- and removing a
-- large clone is not instant:
--
--   T1  DELETE row 15  ---> remove_dir_all(repo-15)  [seconds]
--   T2                INSERT -> rowid 15, path repos/repo-15
--   T1                        ...still deleting, now the new repo's files
--
-- `add_repo` takes no lock, and both servers handle requests
-- concurrently, so nothing prevented that interleaving.
--
-- So deletion becomes: flag the row (one transactional statement, after
-- which the repo is gone from every read path), then reclaim the
-- directory, and only then delete the row -- which is the point at which
-- the id genuinely becomes available again. A crash between the phases
-- leaves a flagged row, which the reaper finishes on the next pass; it
-- never leaves an id that is free while its files are not.
ALTER TABLE repos ADD COLUMN deleted_at INTEGER;

-- The reaper's scan and every read path's filter both hit this. Partial,
-- because the overwhelmingly common state is NULL and those rows do not
-- need indexing -- the filter is served by the table scan the listing
-- does anyway.
CREATE INDEX IF NOT EXISTS repos_deleted_at ON repos(deleted_at)
  WHERE deleted_at IS NOT NULL;
