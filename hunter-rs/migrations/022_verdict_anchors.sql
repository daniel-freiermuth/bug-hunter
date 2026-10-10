-- What a rejecting verdict was judged against.
--
-- A `rejected` or `wontfix` finding is shown to every later scan of its
-- repo as a suppression. Without this, nothing said WHEN a rejection was
-- true: the code it relied on could change and the suppression would go
-- on hiding a finding that had become correct (finding 20: a phantom row
-- hidden only by an unrelated first-bin trim).
--
-- One row per `rejected` finding whose verdict a worker gave: a `wrong`
-- decline or closed-PR harvest, an `invalid` recheck. `wontfix` has none:
-- it is the project's decision, which no code change lapses. `sha` is the
-- commit the verdict was judged at; `files` (a JSON array of repo-relative
-- paths) is the finding's own file plus the ones the worker said the
-- verdict depends on. When building the suppression list, an entry whose
-- files changed between `sha` and the scanned commit is marked CHANGED:
-- the scan re-checks it, and either files it again (which reopens the
-- finding and removes the row) or reconfirms it (which moves `sha` to the
-- scanned commit). `holds_while` is the worker's one-line statement of the
-- condition that makes the verdict true, shown next to its reason.
--
-- Written with the verdict and replaced or removed with the next one, so
-- a row never outlives the verdict it anchors. A human's verdict through
-- the API has no worker tree, so it has no row: its suppression stays
-- unconditional, as every suppression was before this table.
CREATE TABLE verdict_anchors (
  finding_id  INTEGER PRIMARY KEY REFERENCES findings(id) ON DELETE CASCADE,
  sha         TEXT NOT NULL,
  files       TEXT NOT NULL,             -- JSON array of repo-relative paths
  holds_while TEXT,
  created_at  INTEGER NOT NULL           -- epoch ms
);
