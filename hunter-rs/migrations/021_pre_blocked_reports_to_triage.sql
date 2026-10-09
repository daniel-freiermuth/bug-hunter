-- Blocker reports from before the `blocked` status go back to triage.
--
-- Until 9c0d449 a fix worker's BLOCKED.md rejected its finding: the event
-- read "#<id> blocked by worker: <first line>" and the finding became
-- `rejected` with the report as its reason. A blocker says the work is
-- valid and waits on a prerequisite, so those rows sit in the suppression
-- corpus telling every later scan not to report a finding nobody judged
-- wrong. 9c0d449 added `blocked` for new reports and moved none of the old
-- ones; the verdict API cannot take a finding off `rejected` either.
--
-- `new`, not `blocked`: `blocked` holds a retained checkpoint, and these
-- chains have none. The report stays as verdict_reason, so whoever triages
-- the finding sees what it waits on.
--
-- Matched by construction: the event text exists only on that old path
-- (blocks now log "#<id> blocked; checkpoint retained at ..."). A human
-- verdict logged after the report is a decision taken on it and is kept.
-- Checked against the live database before writing this: 8 rows match,
-- every one with a "# BLOCKED:" reason and no event after the report.
UPDATE findings
SET status = 'new'
WHERE status = 'rejected'
  AND EXISTS (
    SELECT 1 FROM events b
    WHERE b.finding_id = findings.id
      AND b.kind = 'fix'
      AND b.message LIKE '#' || findings.id || ' blocked by worker:%'
      AND NOT EXISTS (
        SELECT 1 FROM events v
        WHERE v.finding_id = findings.id
          AND v.kind = 'verdict'
          AND v.id > b.id
      )
  );
