-- Idle-Token Bug Hunter — store schema v2 (unified findings table)
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS repos (
  id            INTEGER PRIMARY KEY,
  name          TEXT NOT NULL UNIQUE,
  url           TEXT NOT NULL,             -- https or ssh remote
  path          TEXT NOT NULL,             -- local clone path (workRoot/repos/<name>)
  forge          TEXT NOT NULL DEFAULT 'github', -- github | gitlab
  default_branch TEXT NOT NULL DEFAULT 'main',
  last_hunt_sha TEXT,                      -- HEAD at last completed hunt
  last_hunt_at  INTEGER,                   -- epoch ms
  enabled       INTEGER NOT NULL DEFAULT 1,
  added_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS findings (
  id            INTEGER PRIMARY KEY,
  type          TEXT NOT NULL,             -- bug | dep_update | test_gap | refactor
  repo_id       INTEGER NOT NULL REFERENCES repos(id),
  fingerprint   TEXT NOT NULL UNIQUE,
  file          TEXT,
  symbol        TEXT,
  line          INTEGER,
  
  -- Common fields (all types)
  severity      TEXT NOT NULL,             -- high|medium|low
  confidence    REAL NOT NULL,
  summary       TEXT NOT NULL,
  detail        TEXT,
  status        TEXT NOT NULL DEFAULT 'new',
    -- new | queued | fixing | pr_open | merged | rejected | wontfix | note
  pr_url        TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  
  -- Bug-specific fields (nullable for other types)
  bug_class     TEXT,                      -- boundary|error-path|race|contract-drift|leak|logic
  evidence_plan TEXT,
  introduced_by TEXT,
  rung_achieved INTEGER,                   -- 1..3 once fixed; NULL before
  verdict_reason TEXT,                     -- REQUIRED for rejected/wontfix (suppression corpus)
  budget_override TEXT,
  fix_attempts  INTEGER NOT NULL DEFAULT 0, -- consecutive run_fix attempts hitting last_fix_failure
  last_fix_failure TEXT,                   -- fingerprint of the last fix attempt's failure reason
  recheck_attempts INTEGER NOT NULL DEFAULT 0, -- consecutive run_recheck attempts hitting last_recheck_failure
  last_recheck_failure TEXT,               -- fingerprint of the last recheck attempt's failure reason
  
  -- Dep update fields (nullable for other types)
  ecosystem     TEXT,
  package       TEXT,
  current_version TEXT,
  latest_version TEXT,
  update_type   TEXT,                      -- major|minor|patch
  security_advisory TEXT,
  
  -- Test gap fields (nullable for other types)
  missing_tests TEXT,
  test_file     TEXT,
  
  -- Refactoring fields (nullable for other types)
  smell_type    TEXT,
  suggested_refactor TEXT
);
CREATE INDEX IF NOT EXISTS findings_status ON findings(status);
CREATE INDEX IF NOT EXISTS findings_repo ON findings(repo_id, status);
CREATE INDEX IF NOT EXISTS findings_type ON findings(type);
CREATE INDEX IF NOT EXISTS findings_repo_type ON findings(repo_id, type);
CREATE INDEX IF NOT EXISTS findings_type_status ON findings(type, status);

CREATE TABLE IF NOT EXISTS jobs (
  id            INTEGER PRIMARY KEY,
  kind          TEXT NOT NULL,             -- hunt | fix | engage | recheck | harvest | ...
  repo_id       INTEGER NOT NULL REFERENCES repos(id),
  finding_id    INTEGER REFERENCES findings(id),
  state         TEXT NOT NULL DEFAULT 'queued',
    -- queued | running | done | failed | killed | denied
  pid           INTEGER,
  session_file  TEXT,                      -- worker's JSONL ledger path
  cap_tokens    INTEGER,
  tokens_new    INTEGER,                   -- input+output+cacheWrite, from ledger
  calls         INTEGER,
  exit_code     INTEGER,
  killed_reason TEXT,                      -- cap | wallclock | NULL
  notes         TEXT,
  model         TEXT,                      -- model used for this job
  usage_delta   REAL,                      -- 7d used_fraction increase observed during job
  started_at    INTEGER,
  finished_at   INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state);

-- Mirror of budget observations at decision time (ops strip history)
CREATE TABLE IF NOT EXISTS window_log (
  id            INTEGER PRIMARY KEY,
  observed_at   INTEGER NOT NULL,
  limit_id      TEXT NOT NULL,             -- anthropic:5h | anthropic:7d | ...
  used_fraction REAL,
  status        TEXT,
  resets_at     INTEGER,
  source_age_s  INTEGER                    -- staleness of the usage_history row we read
);

-- Empirical (hunter_tokens spent -> used_fraction moved) correlations,
-- recorded incrementally each time a FRESH probe lands (used_fraction
-- actually changed since the last row for this resets_at). Lets budget
-- estimate real remaining tokens instead of the fixed "200k ~= 10% of
-- 5h window" guess -- informational only, never gates a decision: the
-- estimate is confounded by (a) any concurrent non-hunter account
-- activity, whose share of the delta hunter can't see, and (b)
-- Anthropic's used_fraction very likely being a cost-weighted metric,
-- not a linear token count, so the true ratio depends on each job's
-- input/output/cache mix and isn't a single universal constant.
CREATE TABLE IF NOT EXISTS calibration_samples (
  id                  INTEGER PRIMARY KEY,
  observed_at         INTEGER NOT NULL,
  limit_id            TEXT NOT NULL,       -- anthropic:5h | anthropic:7d
  window_resets_at    INTEGER,             -- which window instance this belongs to
  used_fraction_delta REAL NOT NULL,       -- Anthropic-reported change since the prior probe
  hunter_tokens       INTEGER NOT NULL     -- hunter's own tokens_new spent in that gap
);

CREATE TABLE IF NOT EXISTS events (
  id            INTEGER PRIMARY KEY,
  at            INTEGER NOT NULL,
  kind          TEXT NOT NULL,             -- cycle|hunt|fix|engage|verdict|ship|deny|error
  message       TEXT NOT NULL,
  job_id        INTEGER,
  finding_id    INTEGER
);

-- PR engagement state — one row per shipped finding, refreshed by sync_prs.
-- Additive (v2): existing DBs pick it up via CREATE TABLE IF NOT EXISTS.
CREATE TABLE IF NOT EXISTS pr_state (
  finding_id    INTEGER PRIMARY KEY REFERENCES findings(id),
  pr_number     INTEGER,
  state         TEXT,                      -- OPEN | MERGED | CLOSED
  mergeable     TEXT,                      -- MERGEABLE | CONFLICTING | UNKNOWN
  checks        TEXT,                      -- short rollup summary ("2 pass / 1 fail")
  head_ref      TEXT,                      -- PR branch name (push/worktree target)
  last_activity_at INTEGER,                -- newest comment/review timestamp (epoch ms)
  last_engaged_activity_at INTEGER,        -- activity high-water mark we responded to
  needs_attention TEXT,                    -- comma-joined reasons; NULL = calm
  attention_since INTEGER,                 -- epoch ms the CURRENT reason first appeared (unchanged
                                            -- while the reason string stays the same); the real
                                            -- fairness key for list_attention() -- synced_at is
                                            -- refreshed for EVERY pr_open finding EVERY cycle in a
                                            -- fixed order, so it reflects loop iteration order, not
                                            -- how long a PR has genuinely been waiting (regression:
                                            -- recentIP PR #6 hogged 5 straight cycles while PR #3 sat
                                            -- flagged for ~10min, purely because #6 has a higher
                                            -- finding id and got synced first every pass)
  attention_fingerprint TEXT,              -- signature of the CURRENT static (non-comment) problem
                                            -- state -- review decision, conflict, and WHICH checks
                                            -- are failing; refreshed every sync_prs pass, same as
                                            -- mergeable/checks
  addressed_fingerprint TEXT,              -- the fingerprint an engage reply already declined to
                                            -- fix (no commits pushed); sync_prs suppresses re-flagging
                                            -- a static reason whose CURRENT fingerprint still matches
                                            -- this AND whose head_sha still matches addressed_head_sha
                                            -- -- state-based, not time-based: no re-poking a
                                            -- worker explained itself on once, no matter how long the
                                            -- daemon then runs unattended, until the actual situation
                                            -- (which check fails, review state, conflict, or the code
                                            -- itself via a human push) changes.
                                            -- Cleared the moment it does (see run_engage/sync_prs).
  head_sha      TEXT,                      -- PR/MR's head commit SHA as of the last sync
  addressed_head_sha TEXT,                 -- head_sha at the moment addressed_fingerprint was set;
                                            -- a mismatch means new code landed since the decline, so
                                            -- a same-looking static reason is treated as fresh, not
                                            -- suppressed (a human push always deserves a fresh look
                                            -- even if, coincidentally, the same check is still red)
  synced_at     INTEGER,
  harvested_at  INTEGER,                   -- epoch ms run_harvest reviewed this merged PR; NULL = pending
  harvest_attempts INTEGER NOT NULL DEFAULT 0, -- consecutive run_harvest attempts hitting last_harvest_failure
  last_harvest_failure TEXT                -- fingerprint of the last harvest attempt's failure reason
);

-- Single-row snapshot of the daemon loop's own reasoning: what it just
-- did, why it's currently sleeping (or nothing to do), and its best
-- current estimate of when it'll check again. Purely informational
-- (the Status page's "what's happening" panel) -- never read by any
-- scheduling decision.
CREATE TABLE IF NOT EXISTS scheduler_state (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  state         TEXT NOT NULL,             -- idle | denied | error
  detail        TEXT NOT NULL,
  next_wake_at  INTEGER,                   -- epoch ms, best-effort estimate
  updated_at    INTEGER NOT NULL
);
