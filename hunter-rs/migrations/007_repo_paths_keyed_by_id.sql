-- Repo clone directories move from repos/<name> to repos/repo-<id>.
--
-- A name is a display string the operator typed; a directory name is not.
-- Two names differing only in case are one directory on NTFS and APFS,
-- Windows reserves CON/NUL/AUX/COM1 and strips trailing dots, and past
-- 255 bytes a name is ENAMETOOLONG -- which surfaced at clone time, long
-- after the repo had been accepted. Validation could only ever be a
-- denylist against an open set, so the name stops being load-bearing and
-- the id owns the path, as NOTES.md already did.
--
-- Rewritten by string surgery rather than by rebuilding the path from a
-- root, because work_root is not knowable from inside SQL: strip the
-- trailing "<name>" and append "repo-<id>". The guard in WHERE means a
-- row whose path does not end in its own name -- an operator-edited path,
-- or one already migrated -- is left exactly as it is.
--
-- The directories themselves are renamed by the deploy, not here: SQL
-- cannot move files, and a half-applied rename is worse than none. A repo
-- whose directory was not moved simply re-clones on its next job, since
-- sync_repo clones when the path does not exist.
UPDATE repos
SET path = substr(path, 1, length(path) - length(name)) || 'repo-' || id
WHERE path LIKE '%/' || name;
