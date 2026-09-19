-- Change fingerprint uniqueness from (fingerprint) to (type, fingerprint).
-- SQLite cannot alter constraints, so we rebuild the table.
-- Note: foreign_keys is OFF by default in SQLite connections; sqlx does not
-- enable it, so the RENAME is safe without PRAGMA foreign_keys = OFF.

ALTER TABLE findings RENAME TO _findings_old;

CREATE TABLE findings (
  id            INTEGER PRIMARY KEY,
  type          TEXT NOT NULL,
  repo_id       INTEGER NOT NULL REFERENCES repos(id),
  fingerprint   TEXT NOT NULL,
  file          TEXT,
  symbol        TEXT,
  line          INTEGER,
  severity      TEXT NOT NULL,
  confidence    REAL NOT NULL,
  summary       TEXT NOT NULL,
  detail        TEXT,
  status        TEXT NOT NULL DEFAULT 'new',
  pr_url        TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  bug_class     TEXT,
  evidence_plan TEXT,
  introduced_by TEXT,
  rung_achieved INTEGER,
  verdict_reason TEXT,
  budget_override TEXT,
  fix_attempts  INTEGER NOT NULL DEFAULT 0,
  last_fix_failure TEXT,
  recheck_attempts INTEGER NOT NULL DEFAULT 0,
  last_recheck_failure TEXT,
  ecosystem     TEXT,
  package       TEXT,
  current_version TEXT,
  latest_version TEXT,
  update_type   TEXT,
  security_advisory TEXT,
  missing_tests TEXT,
  test_file     TEXT,
  smell_type    TEXT,
  suggested_refactor TEXT,
  modernization_class TEXT,
  current_approach TEXT,
  proposed_approach TEXT,
  UNIQUE(type, fingerprint)
);

INSERT INTO findings SELECT * FROM _findings_old;

DROP TABLE _findings_old;

CREATE INDEX IF NOT EXISTS findings_status ON findings(status);
CREATE INDEX IF NOT EXISTS findings_repo ON findings(repo_id, status);
CREATE INDEX IF NOT EXISTS findings_type ON findings(type);
CREATE INDEX IF NOT EXISTS findings_repo_type ON findings(repo_id, type);
CREATE INDEX IF NOT EXISTS findings_type_status ON findings(type, status);
