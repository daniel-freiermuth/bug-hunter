-- Timelines are looked up by finding id on every GET /api/findings, which
-- the UI polls. Without an index the lookup is a full scan of `events`, a
-- table that grows for the life of the service and is already the largest
-- in the database — so the cost of listing findings grew with total
-- history rather than with the number of findings returned.
--
-- Partial: rows with no finding_id are the majority (cycle, deny, repo
-- events) and are never looked up this way, so they stay out of the index.

CREATE INDEX IF NOT EXISTS events_finding ON events(finding_id) WHERE finding_id IS NOT NULL;
