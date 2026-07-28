-- Idle-Token Bug Hunter — store schema v1 (findings schema v1 from exp1 Q6)
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
  repo_id       INTEGER NOT NULL REFERENCES repos(id),
  fingerprint   TEXT NOT NULL UNIQUE,      -- repo:path:symbol:bug-class
  file          TEXT NOT NULL,
  symbol        TEXT,
  line          INTEGER,
  bug_class     TEXT NOT NULL,             -- boundary|error-path|race|contract-drift|leak|logic
  severity      TEXT NOT NULL,             -- high|medium|low
  confidence    REAL NOT NULL,
  summary       TEXT NOT NULL,
  detail        TEXT,
  evidence_plan TEXT,
  rung_achieved INTEGER,                   -- 1..3 once fixed; NULL before
  introduced_by TEXT,
  status        TEXT NOT NULL DEFAULT 'new',
    -- new | queued | fixing | pr_open | merged | rejected | wontfix | note
  verdict_reason TEXT,                     -- REQUIRED for rejected/wontfix (suppression corpus)
  pr_url        TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS findings_status ON findings(status);
CREATE INDEX IF NOT EXISTS findings_repo ON findings(repo_id, status);

CREATE TABLE IF NOT EXISTS jobs (
  id            INTEGER PRIMARY KEY,
  kind          TEXT NOT NULL,             -- hunt | fix | engage | recheck
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
  synced_at     INTEGER
);
