-- Capacity estimation sums token spend inside each past window, which is
-- a range scan on finished_at over every job the daemon has ever run.
-- `status_html` and `decide` both need it, on the endpoint the UI polls,
-- so without an index the cost of drawing the dashboard grows with the
-- job table.
--
-- Partial: a job with no tokens_new contributes nothing to the sum, and
-- rows are only ever summed once finished.

CREATE INDEX IF NOT EXISTS jobs_finished_at ON jobs(finished_at)
  WHERE finished_at IS NOT NULL AND tokens_new IS NOT NULL;
