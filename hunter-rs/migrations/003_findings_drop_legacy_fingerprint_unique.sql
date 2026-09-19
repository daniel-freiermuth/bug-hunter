-- no-transaction
-- Drop the legacy single-column UNIQUE on findings.fingerprint.
--
-- Migration 002 could only ADD the composite unique index; SQLite will not
-- DROP the implicit autoindex behind an inline column constraint. Migration
-- 001 declares `fingerprint TEXT NOT NULL UNIQUE`, so EVERY database --
-- fresh or upgraded -- still carries it after 002. That rejects two
-- findings of DIFFERENT types sharing a fingerprint, exactly what
-- (type, fingerprint) uniqueness exists to allow, so the rebuild below is
-- required on every database rather than an upgrade-only repair.
--
-- (002's own comment claims the initial schema already has the composite
-- form. That was true while 001 was a symlink to hunter/schema.sql; it
-- went stale when 001 was frozen as a copy of the deployed schema.)
--
-- SQLite cannot alter a constraint, so the table is rebuilt. The column
-- list must stay identical to hunter/schema.sql, or fresh and upgraded
-- databases diverge.

-- Keep SQLite from repointing jobs.finding_id / pr_state.finding_id at the
-- renamed table: with legacy_alter_table OFF, RENAME rewrites referencing
-- FK clauses to follow the old table.
PRAGMA legacy_alter_table = ON;
PRAGMA foreign_keys = OFF;

ALTER TABLE findings RENAME TO _findings_old;

CREATE TABLE findings (
  id INTEGER PRIMARY KEY, type TEXT NOT NULL,
  repo_id INTEGER NOT NULL REFERENCES repos(id),
  fingerprint TEXT NOT NULL, file TEXT, symbol TEXT, line INTEGER,
  severity TEXT NOT NULL, confidence REAL NOT NULL,
  summary TEXT NOT NULL, detail TEXT,
  status TEXT NOT NULL DEFAULT 'new', pr_url TEXT,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  bug_class TEXT, evidence_plan TEXT, introduced_by TEXT,
  rung_achieved INTEGER, verdict_reason TEXT, budget_override TEXT,
  fix_attempts INTEGER NOT NULL DEFAULT 0, last_fix_failure TEXT,
  recheck_attempts INTEGER NOT NULL DEFAULT 0, last_recheck_failure TEXT,
  ecosystem TEXT, package TEXT, current_version TEXT, latest_version TEXT,
  update_type TEXT, security_advisory TEXT,
  missing_tests TEXT, test_file TEXT,
  smell_type TEXT, suggested_refactor TEXT,
  modernization_class TEXT, current_approach TEXT, proposed_approach TEXT,
  UNIQUE(type, fingerprint)
);

INSERT INTO findings (
  id, type, repo_id, fingerprint, file, symbol, line,
  severity, confidence, summary, detail, status, pr_url,
  created_at, updated_at, bug_class, evidence_plan,
  introduced_by, rung_achieved, verdict_reason, budget_override,
  fix_attempts, last_fix_failure, recheck_attempts, last_recheck_failure,
  ecosystem, package, current_version, latest_version,
  update_type, security_advisory, missing_tests, test_file,
  smell_type, suggested_refactor,
  modernization_class, current_approach, proposed_approach
) SELECT
  id, type, repo_id, fingerprint, file, symbol, line,
  severity, confidence, summary, detail, status, pr_url,
  created_at, updated_at, bug_class, evidence_plan,
  introduced_by, rung_achieved, verdict_reason, budget_override,
  COALESCE(fix_attempts, 0), last_fix_failure,
  COALESCE(recheck_attempts, 0), last_recheck_failure,
  ecosystem, package, current_version, latest_version,
  update_type, security_advisory, missing_tests, test_file,
  smell_type, suggested_refactor,
  modernization_class, current_approach, proposed_approach
FROM _findings_old;

DROP TABLE _findings_old;

CREATE INDEX IF NOT EXISTS findings_status ON findings(status);
CREATE INDEX IF NOT EXISTS findings_repo ON findings(repo_id, status);
CREATE INDEX IF NOT EXISTS findings_type ON findings(type);
CREATE INDEX IF NOT EXISTS findings_repo_type ON findings(repo_id, type);
CREATE INDEX IF NOT EXISTS findings_type_status ON findings(type, status);

PRAGMA legacy_alter_table = OFF;
PRAGMA foreign_keys = ON;
