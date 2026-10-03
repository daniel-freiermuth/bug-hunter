-- Local user accounts and their login sessions (future.md Phase 3, M1).
--
-- Accounts are created by the operator from the CLI (`hunter user add`);
-- there is no sign-up. A disabled account keeps its row, so the events it
-- authored still name it: `disabled_at` replaces deletion.
--
-- `username` compares case-insensitively, so `Daniel` and `daniel` cannot
-- be two accounts that a login form would confuse.
CREATE TABLE users (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
  password_hash TEXT NOT NULL,             -- argon2id PHC string
  created_at    INTEGER NOT NULL,          -- epoch ms
  disabled_at   INTEGER                    -- epoch ms; NULL = may log in
);

-- One row per logged-in browser. The cookie carries a random token; only
-- its SHA-256 is stored, so a copy of the database logs nobody in.
CREATE TABLE sessions (
  token_hash    BLOB PRIMARY KEY,
  user_id       INTEGER NOT NULL REFERENCES users(id),
  created_at    INTEGER NOT NULL,          -- epoch ms
  expires_at    INTEGER NOT NULL           -- epoch ms
);
CREATE INDEX sessions_user ON sessions(user_id);

-- Who did it, for events a person caused through the API (verdicts,
-- notes, overrides, ...). NULL for the scheduler's own events and for
-- everything recorded before accounts existed.
ALTER TABLE events ADD COLUMN user_id INTEGER REFERENCES users(id);
