-- An operator's request for a full re-hunt of one repo.
--
-- Set by POST /api/repo/rehunt to the request time, or one past the last
-- value issued or attempted if that is later: the value identifies the
-- request and never repeats. Asking again replaces it. While set, the next cold hunt
-- of the repo reviews its complete history (the same scope as the
-- periodic hunt.rehuntDays re-hunt). NULL means no request is pending.
ALTER TABLE repos ADD COLUMN full_hunt_requested_at INTEGER;

-- The request (its full_hunt_requested_at value) a hunt last started to
-- answer, whether or not that hunt got as far as creating a job.
--
-- A request differing from this is unanswered, and the rotation runs the
-- repo's hunt ahead of everything else for it. Written by the hunt that
-- read the request, so a request landing while some other hunt is being
-- prepared or run stays unanswered. Written once the hunt creates its
-- job, or when it fails before that (a fetch error), so a hunt that keeps
-- failing is retried after the scan interval rather than every cycle; a
-- hunt the budget gate turns away has attempted nothing.
ALTER TABLE repos ADD COLUMN full_hunt_request_attempted INTEGER;

-- Whether a hunt chain reviews the repo's complete history (its diff
-- range starts at git's empty tree), and which full re-hunt request it
-- accepted (the full_hunt_requested_at value it read; NULL if none).
--
-- Decided once, when the chain's cold job starts; a resumed attempt copies
-- both from the attempt it continues at INSERT, as with pinned_sha,
-- because a resume cannot re-derive them: the repo row that decided them
-- may have changed since. A finished full-history chain records a full
-- hunt, and settles the pending request only if it is the one the chain
-- accepted.
--
-- 0 / NULL for every other job, and for every row written before these
-- columns existed.
ALTER TABLE jobs ADD COLUMN full_history INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN full_hunt_request INTEGER;
