-- Change fingerprint uniqueness from (fingerprint) to (type, fingerprint).
-- On a fresh DB (build.rs replay), the initial schema already has the
-- composite constraint, so this is a no-op. On an existing DB upgraded
-- from the old inline UNIQUE, the autoindex needs replacing — but SQLite
-- won't DROP an autoindex, so we use CREATE INDEX IF NOT EXISTS which
-- succeeds either way (the composite index either already exists from
-- the initial schema, or gets created here for upgraded DBs).
CREATE UNIQUE INDEX IF NOT EXISTS findings_type_fingerprint ON findings(type, fingerprint);
