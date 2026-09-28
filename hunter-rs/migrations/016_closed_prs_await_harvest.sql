-- Closures from before the closed-PR harvest wait for it like any other.
--
-- A PR closed without merging now leaves its finding `closed` until the
-- one-time closed-PR harvest classifies the closure, and the harvest
-- selects on `closed` alone. Before that, every closure -- sync seeing the
-- PR closed, or an engage worker withdrawing it -- set the finding
-- `rejected` instead. So a `rejected` finding whose PR is CLOSED and not
-- yet harvested is a closure's rejection by construction, and this moves
-- exactly those to `closed` so they get their harvest.
--
-- The harvest cannot select on `rejected` itself: a human can set
-- `rejected` through the verdict API after a PR closed, and that verdict
-- is indistinguishable from a closure's in the row, so the harvest's
-- classification would overwrite it. `closed` carries no such ambiguity,
-- because the verdict API cannot set it. Checked against the live
-- database before writing this: all 16 rows it matches carry a closure's
-- reason, and none received a manual verdict after its PR closed.
--
-- verdict_reason stays as it is: it is the closure's own reason (the
-- worker's withdrawal text, or "PR closed without merge ..."), which the
-- harvest reads as its starting point.
UPDATE findings
SET status = 'closed'
WHERE status = 'rejected'
  AND id IN (
    SELECT finding_id FROM pr_state
    WHERE state = 'CLOSED' AND harvested_at IS NULL
  );
