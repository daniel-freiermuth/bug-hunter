-- Which job produced a finding.
--
-- Nothing recorded this. The ingest path had the job id in hand and
-- passed it to every event it logged except the one that matters -- the
-- "new <type>" event marking the finding's creation got NULL -- so the
-- job that found a bug and the bug itself were never connected in the
-- data at all. Asking "what did this hunt turn up?" meant reading the
-- "+2 new" counter in a log message and searching by timestamp.
--
-- Provenance belongs on the finding rather than inferred from event
-- text: a finding is produced by exactly one job, and matching events by
-- message prefix would break the first time the wording changed.
--
-- Rows that already exist keep NULL. The job that found them is not
-- recoverable -- the information was dropped when they were ingested,
-- not merely unindexed -- so the column is nullable by necessity and the
-- UI shows nothing for them rather than guessing.
--
-- No foreign key to jobs(id): jobs are pruned on a retention policy that
-- knows nothing about findings, and a finding must outlive the job that
-- found it rather than block its cleanup.
ALTER TABLE findings ADD COLUMN found_by_job INTEGER;

CREATE INDEX findings_found_by_job ON findings(found_by_job)
  WHERE found_by_job IS NOT NULL;
