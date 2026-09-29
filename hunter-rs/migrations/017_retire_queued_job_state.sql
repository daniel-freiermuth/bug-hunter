-- Close out the job rows left in `queued`, the last users of that state.
--
-- `queued` was the Python daemon's insert default: create_job wrote the
-- row as `queued` and flipped it to `running` once prep was done. Since
-- 395d2a500 (2026-09-06) every caller inserted `running` directly, and
-- the Rust daemon never writes `queued` at all. The only `queued` rows
-- are ones whose cycle crashed between insert and start -- in the live
-- database, 309 hunt jobs from 2026-08-08..10, one per "unfilled
-- placeholder in playbook" crash. No worker ever ran for any of them.
--
-- They become `failed`, finished at their start, with no tokens: every
-- budget and history query requires tokens_new, so none of them starts
-- counting. The column default stays 'queued' because changing a default
-- means rebuilding `jobs`, and no insert relies on it.
UPDATE jobs
SET state = 'failed',
    finished_at = COALESCE(finished_at, started_at),
    notes = COALESCE(notes || ' | ', '') ||
            'never started: the cycle crashed after the job row was created'
WHERE state = 'queued';
