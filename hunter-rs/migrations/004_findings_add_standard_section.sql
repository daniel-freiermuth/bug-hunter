-- standards findings record WHICH standard was violated.
--
-- Every other finding type has a class column feeding Finding::category()
-- (bug_class, update_type, smell_type, modernization_class); standards was
-- the one type with none, so the playbook's `standard_section` had nowhere
-- to land and ingest demanded refactor's `smell_type` instead -- a field the
-- standards playbook never emits, which rejected every standards finding.
--
-- Additive: SQLite applies ADD COLUMN atomically, and existing rows read
-- NULL, so this is safe on a populated database.

ALTER TABLE findings ADD COLUMN standard_section TEXT;
