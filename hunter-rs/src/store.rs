//! Data access against the live hunter.db (WAL mode).
//!
//! # Conventions
//! - Every query uses an EXPLICIT column list — never `SELECT *`.
//! - Mutation methods use **specific, purpose-named methods** with
//!   `sqlx::query!` macros — fully compile-time checked SQL, no
//!   `QueryBuilder` for mutations.
//! - One method, one unit of work: a handler never opens a transaction of
//!   its own or spans one across two `Store` calls. Most methods are a
//!   single statement and autocommit. A method whose statements must land
//!   together owns its transaction internally -- `add_repo` (the insert and
//!   the id-derived clone path), `soft_delete_repo` (the findings/jobs
//!   refusal checks and the flag, under `BEGIN IMMEDIATE` so the checks
//!   cannot go stale before the write), and `sync_pr_open` (the PR upsert
//!   and the dependent `attention_since` update). Those transactions close
//!   real races; do not unwind them to make a method look like the others.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::domain::{
    BugClass, FindingStatus, FindingType, ForgeName, JobKind, JobState, RepoJobKind, Severity,
};
use crate::types::{
    Event, Finding, Job, JobListEntry, PrState, Repo, SchedulerState, StatsByFinding, StatsByKind,
    StatsTotals,
};
use crate::util::now_ms;

/// Write-path errors: a domain refusal (HTTP 400 with the exact message)
/// vs an underlying DB error (HTTP 500).
#[derive(Debug, thiserror::Error)]
pub enum StoreWriteError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Allowed /api/repo update fields, post-coercion (WRITES contract §6).
#[derive(Debug, Default)]
pub struct RepoUpdate {
    pub enabled: Option<i64>,
    pub url: Option<String>,
    pub default_branch: Option<String>,
    pub forge: Option<ForgeName>,
    pub name: Option<String>,
}

/// All required fields for syncing an open PR's state.  The only
/// conditional parts — `attention_since` and `clear_addressed` — are
/// handled by two extra compile-time checked queries inside
/// `sync_pr_open`, not dynamic SQL.
#[derive(Debug)]
pub struct SyncPrData {
    pub pr_number: i64,
    pub state: String,
    pub mergeable: String,
    pub checks: Option<String>,
    pub head_ref: String,
    pub head_sha: String,
    pub last_activity_at: i64,
    pub last_engaged_activity_at: i64,
    pub needs_attention: Option<String>,
    pub attention_fingerprint: Option<String>,
    pub synced_at: i64,
    /// None = don't touch; Some(None) = set NULL; Some(Some(v)) = set to v.
    pub attention_since: Option<Option<i64>>,
    /// When true, set `addressed_fingerprint` = NULL, `addressed_head_sha` = NULL.
    pub clear_addressed: bool,
}

/// Typed insert for `upsert_finding` — compile-time field safety instead of
/// runtime `.get()` on raw JSON.
#[derive(Debug, Clone, Default)]
pub struct FindingInsert {
    pub fingerprint: String,
    pub file: String,
    pub symbol: Option<String>,
    pub line: Option<i64>,
    pub severity: String,
    pub confidence: f64,
    pub summary: String,
    pub detail: Option<String>,
    pub bug_class: Option<String>,
    pub evidence_plan: Option<String>,
    pub introduced_by: Option<String>,
    pub ecosystem: Option<String>,
    pub package: Option<String>,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub update_type: Option<String>,
    pub security_advisory: Option<String>,
    pub missing_tests: Option<String>,
    pub test_file: Option<String>,
    pub smell_type: Option<String>,
    pub suggested_refactor: Option<String>,
    pub modernization_class: Option<String>,
    pub current_approach: Option<String>,
    pub proposed_approach: Option<String>,
    pub standard_section: Option<String>,
}

/// Typed update for finding analysis fields after worker recheck.
#[derive(Debug, Clone, Default)]
pub struct FindingAnalysisUpdate {
    pub summary: Option<String>,
    pub detail: Option<String>,
    pub confidence: Option<f64>,
    pub severity: Option<String>,
}

/// Filters for GET /api/findings (all optional, combined with AND).
#[derive(Debug, Default)]
pub struct FindingFilter {
    pub status: Option<FindingStatus>,
    pub repo_id: Option<i64>,
    pub kind: Option<FindingType>,
    /// Minimum severity rank (`Severity::rank`); expands to "at or above".
    pub min_severity_rank: Option<i64>,
}

/// Tail-truncation limit for repo notes (`Store._MAX_NOTES_CHARS`).
const MAX_NOTES_CHARS: usize = 4000;
const NOTES_TRUNCATION_PREFIX: &str = "...(older notes truncated)...\n";

/// (YYYY-MM-DD, HH:MM) in UTC for repo-note timestamps. Python uses
/// `datetime.now()` (LOCAL time) here; deriving the local offset without a
/// time crate isn't worth it, so we store UTC — an accepted, documented
/// deviation (notes are informational free text, never parsed).
fn utc_date_time() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{:02}:{:02}", tod / 3_600, (tod % 3_600) / 60),
    )
}

/// Proleptic-Gregorian date from days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, exact for the full i64-day range we can encounter).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    // Private — all SQL goes through typed store methods.
    // No code outside this module can access the raw pool (field is not pub).

    // ── counting helpers (daemon sleep logic) ─────────────────────────

    pub async fn count_queued(&self) -> sqlx::Result<i64> {
        let status = FindingStatus::Queued;
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM findings WHERE status = ?1"#,
            status
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn count_enabled_repos(&self) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM repos WHERE enabled = 1 AND deleted_at IS NULL"#
        )
        .fetch_one(&self.pool)
        .await
    }

    // ── repo timestamp helpers (scheduler) ────────────────────────────

    /// Clear `last_hunt_sha` (triggers a full re-hunt on next cycle).
    pub async fn clear_last_hunt_sha(&self, repo_id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE repos SET last_hunt_sha = NULL WHERE id = ?1",
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set `last_dep_update_at` to now.
    pub async fn set_last_dep_update(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_dep_update_at = ?1 WHERE id = ?2",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set `last_standards_at` to now.
    pub async fn set_last_standards_at(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_standards_at = ?1 WHERE id = ?2",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Advance a rotation-kind's `last_{kind}_at` timestamp.
    /// Uses per-kind compile-time checked queries (no `QueryBuilder`).
    pub async fn set_last_kind_at(&self, repo_id: i64, kind: RepoJobKind) -> sqlx::Result<()> {
        let now = now_ms();
        match kind {
            RepoJobKind::Hunt => {
                sqlx::query!(
                    "UPDATE repos SET last_hunt_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::TestGap => {
                sqlx::query!(
                    "UPDATE repos SET last_test_gap_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::DepUpdate => {
                sqlx::query!(
                    "UPDATE repos SET last_dep_update_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::Refactor => {
                sqlx::query!(
                    "UPDATE repos SET last_refactor_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::Modernization => {
                sqlx::query!(
                    "UPDATE repos SET last_modernization_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::Standards => {
                sqlx::query!(
                    "UPDATE repos SET last_standards_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }
}

/// The embedded migration chain.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

impl Store {
    /// Open the DB strictly read-only -- physically unable to write, so a
    /// caller that must not mutate cannot. Used by the tests to assert a
    /// read path touches nothing.
    pub async fn connect_read_only(db_path: &Path) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .read_only(true)
            .create_if_missing(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        Ok(Self { pool })
    }

    /// Open the live DB read-write. Runs embedded migrations on connect —
    /// the binary carries its own schema, so deploying a new binary
    /// automatically migrates the DB.
    pub async fn connect(db_path: &Path) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            // Must be set here rather than by migration 001's `PRAGMA
            // journal_mode = WAL`: sqlx runs each migration inside a
            // transaction and SQLite refuses to change journal mode there,
            // so on a database that is not already WAL that migration
            // fails outright. As a connect option it runs outside any
            // transaction, and 001's pragma is then the no-op it has to be.
            // 001 cannot simply drop the pragma: it is already applied
            // everywhere, and sqlx rejects a migration whose text changed.
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    // -- writes (API-CONTRACT-WRITES.md; commit-per-method) -------------------

    /// SELECT all 39 columns FROM findings WHERE id = ? (embedded row in
    /// verdict/recheck/unqueue/override success bodies).
    pub async fn get_finding(&self, id: i64) -> sqlx::Result<Option<Finding>> {
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override, fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE id = ?1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Plain finding status transition (new, queued, rechecking, merged, fixing).
    pub async fn set_finding_status(
        &self,
        finding_id: i64,
        status: FindingStatus,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET status = ?1, updated_at = ?2 WHERE id = ?3",
            status,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Status transition with a verdict reason (rejected, wontfix).
    pub async fn set_finding_verdict(
        &self,
        finding_id: i64,
        status: FindingStatus,
        reason: &str,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 WHERE id = ?4",
            status,
            reason,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set finding status to `pr_open` with PR URL.
    pub async fn set_finding_pr_open(&self, finding_id: i64, pr_url: &str) -> sqlx::Result<()> {
        let now = now_ms();
        let pr_open = FindingStatus::PrOpen;
        sqlx::query!(
            "UPDATE findings SET status = ?1, pr_url = ?2, updated_at = ?3 WHERE id = ?4",
            pr_open,
            pr_url,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE findings SET `budget_override` = ?, `updated_at` = now WHERE id = ?.
    /// mode: Some("once") | Some("exempt") | None (clear).
    pub async fn set_budget_override(
        &self,
        finding_id: i64,
        mode: Option<&str>,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET budget_override = ?1, updated_at = ?2 WHERE id = ?3",
            mode,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE findings SET `budget_override` = NULL, `updated_at` = now
    /// WHERE `budget_override` IS NOT NULL; returns rows affected.
    pub async fn clear_all_overrides(&self) -> sqlx::Result<i64> {
        let now = now_ms();
        let result = sqlx::query!(
            "UPDATE findings SET budget_override = NULL, updated_at = ?1 \
             WHERE budget_override IS NOT NULL",
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() as i64)
    }

    /// Directory a repo is cloned into: `<repos_dir>/repo-<id>`.
    ///
    /// Derived from the id, never from the name. A name is whatever the
    /// operator typed, and a filesystem is not a string store: names
    /// differing only in case collide on NTFS and APFS, `CON` and `NUL`
    /// are reserved on Windows, trailing dots are silently stripped
    /// there, and anything past 255 bytes is `ENAMETOOLONG` — which would
    /// surface at hunt time, long after the repo was accepted. Every
    /// charset rule that could be written here is a denylist against an
    /// open-ended set, so the name is display-only and the id owns the
    /// path. `NOTES.md` has always been keyed this way.
    pub fn repo_dir(repos_dir: &Path, repo_id: i64) -> PathBuf {
        repos_dir.join(format!("repo-{repo_id}"))
    }

    /// Where a repo's notes live: `<work_root>/notes/repo-<id>.md`.
    ///
    /// Outside the clone, and deliberately so. Notes used to be written
    /// to `repos/repo-<id>/NOTES.md`, which put them inside the working
    /// tree of a real git checkout, with two consequences.
    ///
    /// The file showed up as untracked in the clone, so any worker doing
    /// a broad `git add` would commit the operator's private notes into
    /// a pull request.
    ///
    /// And writing a note created `repos/repo-<id>/` as a side effect.
    /// `sync_repo` treats an existing path as an already-cloned repo and
    /// checks its `origin`; a directory holding only notes has no origin
    /// to read, so it refused to work there — permanently, since nothing
    /// removes it. Adding a note to a repo before its first cycle was
    /// enough to make that repo uncloneable for good.
    ///
    /// Keyed by id for the same reason as the clone directory: the name
    /// is whatever the operator typed.
    pub fn notes_path(work_root: &Path, repo_id: i64) -> PathBuf {
        work_root.join("notes").join(format!("repo-{repo_id}.md"))
    }

    /// INSERT INTO repos (name, url, path, forge, `default_branch`, `added_at`);
    /// returns new id. `path` is `<repos_dir>/repo-<id>`, so it can only be
    /// written once the id exists — both statements share a transaction to
    /// rule out a row whose path never got filled in.
    pub async fn add_repo(
        &self,
        name: &str,
        url: &str,
        repos_dir: &Path,
        default_branch: &str,
        forge: ForgeName,
    ) -> sqlx::Result<i64> {
        let added_at = now_ms();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query!(
            "INSERT INTO repos (name, url, path, forge, default_branch, added_at) \
             VALUES (?1, ?2, '', ?3, ?4, ?5)",
            name,
            url,
            forge,
            default_branch,
            added_at
        )
        .execute(&mut *tx)
        .await?;
        let id = result.last_insert_rowid();
        let path = Self::repo_dir(repos_dir, id).to_string_lossy().into_owned();
        sqlx::query!("UPDATE repos SET path = ?1 WHERE id = ?2", path, id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// UPDATE repos — each field is set only when non-NULL, else the
    /// existing value is preserved via COALESCE.
    pub async fn update_repo(&self, id: i64, fields: &RepoUpdate) -> sqlx::Result<()> {
        if fields.enabled.is_none()
            && fields.url.is_none()
            && fields.default_branch.is_none()
            && fields.forge.is_none()
            && fields.name.is_none()
        {
            return Ok(());
        }
        let url = fields.url.as_deref();
        let branch = fields.default_branch.as_deref();
        let forge = fields.forge.map(super::domain::ForgeName::as_str);
        let name = fields.name.as_deref();
        sqlx::query!(
            "UPDATE repos SET \
             enabled = COALESCE(?1, enabled), \
             url = COALESCE(?2, url), \
             default_branch = COALESCE(?3, default_branch), \
             forge = COALESCE(?4, forge), \
             name = COALESCE(?5, name) \
             WHERE id = ?6",
            fields.enabled,
            url,
            branch,
            forge,
            name,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Refuses (exact Python message: "repo <id> has <n> finding(s) and
    /// <m> job(s) -- cannot delete without losing history; pause it
    /// instead") when findings or jobs reference the repo; else flags the
    /// row `deleted_at = now`, which removes it from every read path.
    ///
    /// Phase one of two. The row deliberately survives its own deletion:
    /// it is the only record that `repos/repo-<id>` is still on disk, and
    /// removing a large clone is not instant. [`reap_deleted_repos`]
    /// removes the directory and drops the row only once that succeeded,
    /// so an interrupted or refused removal leaves a flagged row the next
    /// pass retries instead of a directory nothing owns -- and `sync_repo`
    /// treats any directory at that path as an existing clone.
    ///
    /// The count checks and the flag share one transaction, so a job or
    /// finding created concurrently cannot slip in between them.
    pub async fn soft_delete_repo(&self, id: i64) -> Result<(), StoreWriteError> {
        // BEGIN IMMEDIATE rather than sqlx's plain `begin()`: a deferred
        // transaction takes no write lock until its first write, so the
        // counts below would be read outside it and a job inserted
        // concurrently could land between the check and the flag. Taking
        // the lock up front makes the check and the flag see one state.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let findings = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM findings WHERE repo_id = ?1"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        let jobs = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM jobs WHERE repo_id = ?1"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if findings > 0 || jobs > 0 {
            return Err(StoreWriteError::Refused(format!(
                "repo {id} has {findings} finding(s) and {jobs} job(s) -- \
                 cannot delete without losing history; pause it instead"
            )));
        }
        // The name is released here, not at reap time: `repos.name` is
        // UNIQUE, so a flagged row would otherwise keep rejecting the name
        // of a repo the operator has already been told is gone. Suffixing
        // with the id keeps it unique without a table rebuild to drop the
        // constraint, and the row is invisible to every read path anyway --
        // only the reaper looks at it, and only by id.
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos
             SET deleted_at = ?1, name = name || ' (deleted #' || id || ')'
             WHERE id = ?2 AND deleted_at IS NULL",
            now,
            id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Is this id a repo that was deleted but not yet reclaimed?
    ///
    /// Only the delete endpoint asks, so that a retry of a request whose
    /// response was lost is still a success rather than a 404.
    pub async fn repo_is_deleted(&self, id: i64) -> sqlx::Result<bool> {
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM repos
               WHERE id = ?1 AND deleted_at IS NOT NULL"#,
            id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Ids of repos awaiting reclamation, oldest first.
    pub async fn deleted_repo_ids(&self) -> sqlx::Result<Vec<i64>> {
        sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM repos
               WHERE deleted_at IS NOT NULL ORDER BY deleted_at"#
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Drop a flagged row once its directory and notes are gone. The row is
    /// what records that `repos/repo-<id>` and the notes file are still on
    /// disk -- the reaper only finds leftovers through it -- so dropping it
    /// first would orphan them for good. It does not free the id:
    /// `repos.id` is AUTOINCREMENT (migration 009), so SQLite never reissues
    /// it.
    pub async fn forget_deleted_repo(&self, id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "DELETE FROM repos WHERE id = ?1 AND deleted_at IS NOT NULL",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// INSERT INTO events (at = now, kind, message, `job_id`, `finding_id`).
    pub async fn log_event(
        &self,
        kind: &str,
        message: &str,
        job_id: Option<i64>,
        finding_id: Option<i64>,
    ) -> sqlx::Result<()> {
        let at = now_ms();
        sqlx::query!(
            "INSERT INTO events (at, kind, message, job_id, finding_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            at,
            kind,
            message,
            job_id,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append to [`Store::notes_path`] (creating dir +
    /// header "# Notes: {`repo_name}\n\nLast` updated: YYYY-MM-DD\n\n" on
    /// first write), entry "## {category}\n" (when Some) +
    /// "- [{YYYY-MM-DD HH:MM}] {note}\n\n" (UTC — Python wrote local
    /// time; accepted deviation, see `utc_date_time`). Returns the
    /// bounded re-read (same truncation as `repo_notes`).
    pub fn append_repo_note(
        work_root: &Path,
        repo_id: i64,
        repo_name: &str,
        note: &str,
        category: Option<&str>,
    ) -> std::io::Result<String> {
        use std::fmt::Write;
        let path = Self::notes_path(work_root, repo_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (date, hhmm) = utc_date_time();
        let mut entry = String::new();
        if !path.exists() {
            // "Last updated" is written once at creation, never refreshed
            // (`Store.append_repo_note`).
            let _ = write!(entry, "# Notes: {repo_name}\n\nLast updated: {date}\n\n");
        }
        if let Some(category) = category {
            let _ = writeln!(entry, "## {category}");
        }
        let _ = write!(entry, "- [{date} {hhmm}] {note}\n\n");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(entry.as_bytes())?;
        Ok(Self::repo_notes(work_root, repo_id))
    }

    // -- repos ---------------------------------------------------------------

    /// SELECT <cols> FROM repos ORDER BY name (BINARY collation).
    pub async fn list_repos(&self) -> sqlx::Result<Vec<Repo>> {
        sqlx::query_as!(
            Repo,
            r#"
            SELECT id, name, url, path, forge AS "forge: ForgeName", default_branch, last_hunt_sha,
                   last_hunt_at, enabled, added_at, last_full_hunt_at,
                   last_test_gap_at, last_dep_update_at, last_refactor_at,
                   last_modernization_at, last_standards_at
            FROM repos
            WHERE deleted_at IS NULL
            ORDER BY name
            "#
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_repo_by_id(&self, id: i64) -> sqlx::Result<Option<Repo>> {
        sqlx::query_as!(
            Repo,
            r#"
            SELECT id, name, url, path, forge AS "forge: ForgeName", default_branch, last_hunt_sha,
                   last_hunt_at, enabled, added_at, last_full_hunt_at,
                   last_test_gap_at, last_dep_update_at, last_refactor_at,
                   last_modernization_at, last_standards_at
            FROM repos
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn get_repo_by_name(&self, name: &str) -> sqlx::Result<Option<Repo>> {
        sqlx::query_as!(
            Repo,
            r#"
            SELECT id, name, url, path, forge AS "forge: ForgeName", default_branch, last_hunt_sha,
                   last_hunt_at, enabled, added_at, last_full_hunt_at,
                   last_test_gap_at, last_dep_update_at, last_refactor_at,
                   last_modernization_at, last_standards_at
            FROM repos
            WHERE name = ?1 AND deleted_at IS NULL
            "#,
            name
        )
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn finding_exists(&self, id: i64) -> sqlx::Result<bool> {
        sqlx::query_scalar!(r#"SELECT 1 AS "one!: i64" FROM findings WHERE id = ?1"#, id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.is_some())
    }

    /// NOT in the DB: reads [`Store::notes_path`], "" when missing,
    /// tail-truncated to 4000 chars with the
    /// "...(older notes truncated)...\n" prefix (`Store.repo_notes`).
    pub fn repo_notes(work_root: &Path, repo_id: i64) -> String {
        let path = Self::notes_path(work_root, repo_id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return String::new();
        };
        let total = text.chars().count();
        if total <= MAX_NOTES_CHARS {
            return text;
        }
        // Python slices by character (text[-4000:]); mirror that, not bytes.
        let start = text
            .char_indices()
            .nth(total - MAX_NOTES_CHARS)
            .map_or(0, |(i, _)| i);
        let mut out = String::with_capacity(NOTES_TRUNCATION_PREFIX.len() + text.len() - start);
        out.push_str(NOTES_TRUNCATION_PREFIX);
        out.push_str(&text[start..]);
        out
    }

    // -- findings ------------------------------------------------------------

    /// ORDER BY id DESC, no LIMIT. Severity filter via rank CASE expression.
    pub async fn list_findings(&self, filter: &FindingFilter) -> sqlx::Result<Vec<Finding>> {
        let status = filter.status.map(|s| s.as_str().to_owned());
        let status = status.as_deref();
        let kind = filter.kind.map(super::domain::FindingType::as_str);
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override, fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE (?1 IS NULL OR status = ?1)
              AND (?2 IS NULL OR repo_id = ?2)
              AND (?3 IS NULL OR type = ?3)
              AND (?4 IS NULL
                   OR CASE severity
                        WHEN 'high' THEN 3
                        WHEN 'medium' THEN 2
                        ELSE 1
                      END >= ?4)
            ORDER BY id DESC
            "#,
            status,
            filter.repo_id,
            kind,
            filter.min_severity_rank
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Events grouped by `finding_id`, ascending id order within each group.
    /// Timelines for the findings being returned, keyed by finding id.
    ///
    /// Scoped to `ids` rather than reading the whole table: `events` grows
    /// for the life of the service, and this runs on every
    /// `GET /api/findings`, which the UI polls. Unscoped, a filter
    /// matching nothing still paid for the entire history. Python bound
    /// the id list too (`Store.events_by_finding(fids)`).
    ///
    /// The ids go in as a JSON array joined through `json_each`, because
    /// `query_as!` cannot bind a variable-length `IN` list and this keeps
    /// the query compile-time checked.
    pub async fn events_by_finding(&self, ids: &[i64]) -> sqlx::Result<BTreeMap<i64, Vec<Event>>> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids_json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
        let rows = sqlx::query_as!(
            Event,
            r#"
            SELECT e.id AS "id!", e.at AS "at!", e.kind AS "kind!", e.message AS "message!",
                   e.job_id, e.finding_id
            FROM events e
            JOIN json_each(?1) ids ON ids.value = e.finding_id
            "#,
            ids_json
        )
        .fetch_all(&self.pool)
        .await?;
        let mut grouped: BTreeMap<i64, Vec<Event>> = BTreeMap::new();
        for event in rows {
            if let Some(fid) = event.finding_id {
                grouped.entry(fid).or_default().push(event);
            }
        }
        // Ascending id per finding, as the contract requires. Sorted here
        // rather than by `ORDER BY`: the index join yields rows grouped by
        // finding, so ordering in SQL costs a temp B-tree over every row
        // returned, while these per-finding runs are short.
        for events in grouped.values_mut() {
            events.sort_unstable_by_key(|e| e.id);
        }
        Ok(grouped)
    }

    pub async fn get_pr_state(&self, finding_id: i64) -> sqlx::Result<Option<PrState>> {
        sqlx::query_as!(
            PrState,
            r#"
            SELECT finding_id, pr_number, state, mergeable, checks, head_ref,
                   last_activity_at, last_engaged_activity_at, needs_attention,
                   attention_since, attention_fingerprint,
                   addressed_fingerprint, head_sha, addressed_head_sha,
                   synced_at, harvested_at, harvest_attempts,
                   last_harvest_failure
            FROM pr_state
            WHERE finding_id = ?1
            "#,
            finding_id
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// `pr_state` rows for all `pr_open` findings in one query:
    /// `finding_id` -> `needs_attention` (row presence matters, value may be null).
    pub async fn pr_attention(&self) -> sqlx::Result<BTreeMap<i64, Option<String>>> {
        let pr_open = FindingStatus::PrOpen;
        let rows = sqlx::query!(
            r#"
            SELECT p.finding_id AS "finding_id!: i64", p.needs_attention
            FROM pr_state p
            JOIN findings f ON f.id = p.finding_id
            WHERE f.status = ?1
            "#,
            pr_open
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.finding_id, r.needs_attention))
            .collect())
    }

    /// All `FindingStatus` keys zero-filled, then GROUP BY status counts.
    pub async fn status_counts(&self) -> sqlx::Result<BTreeMap<String, i64>> {
        let mut counts: BTreeMap<String, i64> = FindingStatus::ALL
            .iter()
            .map(|s| (s.as_str().to_owned(), 0))
            .collect();
        let rows = sqlx::query!(
            r#"
            SELECT status, COUNT(*) AS "n!: i64"
            FROM findings
            GROUP BY status
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            counts.insert(row.status, row.n);
        }
        Ok(counts)
    }

    /// GROUP BY type — only observed types, possibly empty.
    pub async fn type_counts(&self) -> sqlx::Result<BTreeMap<String, i64>> {
        let rows = sqlx::query!(
            r#"
            SELECT type AS "kind!: String", COUNT(*) AS "n!: i64"
            FROM findings
            GROUP BY type
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| (r.kind, r.n)).collect())
    }

    // -- jobs ----------------------------------------------------------------

    /// jobs JOIN repos, ORDER BY j.id DESC LIMIT ?. finding_* keys stay None.
    /// Recent jobs, each carrying the findings it produced.
    ///
    /// `produced_finding_ids` comes from one correlated subquery rather
    /// than a query per job: this feeds a 50-row table that polls, so an
    /// N+1 here would be 50 extra round trips every few seconds.
    /// `group_concat` returns NULL for a job that produced nothing,
    /// which is the common case (every fix, recheck and engage job), so
    /// the empty vector is the normal result and not an error.
    pub async fn list_jobs(&self, limit: i64) -> sqlx::Result<Vec<JobListEntry>> {
        let rows = sqlx::query!(
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   (SELECT group_concat(f.id) FROM findings f WHERE f.found_by_job = j.id)
                       AS "produced?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            ORDER BY j.id DESC
            LIMIT ?1
            "#,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| JobListEntry {
                produced_finding_ids: r
                    .produced
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|s| s.parse().ok())
                    .collect(),
                job: Job {
                    id: r.id,
                    kind: r.kind,
                    repo_id: r.repo_id,
                    finding_id: r.finding_id,
                    state: r.state,
                    pid: r.pid,
                    session_file: r.session_file,
                    cap_tokens: r.cap_tokens,
                    tokens_new: r.tokens_new,
                    calls: r.calls,
                    exit_code: r.exit_code,
                    killed_reason: r.killed_reason,
                    notes: r.notes,
                    model: r.model,
                    usage_delta: r.usage_delta,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    repo_name: r.repo_name,
                    finding_summary: None,
                    finding_fingerprint: None,
                },
            })
            .collect())
    }

    /// Complete history for one finding, ORDER BY j.id DESC, no limit.
    pub async fn jobs_by_finding(&self, finding_id: i64) -> sqlx::Result<Vec<Job>> {
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE j.finding_id = ?1
            ORDER BY j.id DESC
            "#,
            finding_id
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Newest running job with `finding_summary/finding_fingerprint` populated
    /// via correlated subqueries (NULL -> absent key, see `types::Job`).
    pub async fn current_job(&self) -> sqlx::Result<Option<Job>> {
        let running = JobState::Running;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   (SELECT f.summary FROM findings f WHERE f.id = j.finding_id)
                       AS "finding_summary?: String",
                   (SELECT f.fingerprint FROM findings f WHERE f.id = j.finding_id)
                       AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE j.state = ?1
            ORDER BY j.id DESC
            LIMIT 1
            "#,
            running
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Running jobs only, filtered in SQL — reconcile must not drag the whole
    /// job history into memory. Same join and newest-first ordering as
    /// `list_jobs`; finding_* keys stay None.
    pub async fn list_running_jobs(&self) -> sqlx::Result<Vec<Job>> {
        let running = JobState::Running;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE j.state = ?1
            ORDER BY j.id DESC
            "#,
            running
        )
        .fetch_all(&self.pool)
        .await
    }

    // -- events / scheduler ----------------------------------------------------

    /// ORDER BY id DESC LIMIT ?.
    pub async fn recent_events(&self, limit: i64) -> sqlx::Result<Vec<Event>> {
        sqlx::query_as!(
            Event,
            r#"
            SELECT id, at, kind, message, job_id, finding_id
            FROM events
            ORDER BY id DESC
            LIMIT ?1
            "#,
            limit
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Newest event with kind = 'cycle' (accepted deviation from Python's
    /// "within the last 500 events" window — strictly more correct).
    pub async fn last_cycle_event(&self) -> sqlx::Result<Option<Event>> {
        sqlx::query_as!(
            Event,
            r#"
            SELECT id, at, kind, message, job_id, finding_id
            FROM events
            WHERE kind = 'cycle'
            ORDER BY id DESC
            LIMIT 1
            "#
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// SELECT ... FROM `scheduler_state` WHERE id = 1.
    pub async fn scheduler_state(&self) -> sqlx::Result<Option<SchedulerState>> {
        sqlx::query_as!(
            SchedulerState,
            r#"
            SELECT id, state, detail, next_wake_at, updated_at
            FROM scheduler_state
            WHERE id = 1
            "#
        )
        .fetch_optional(&self.pool)
        .await
    }

    // -- scheduler store methods ----------------------------------------------

    /// INSERT INTO jobs, returns new id. state starts as "running" (the
    /// scheduler immediately overwrites to the caller's desired state).
    ///
    /// Refuses when the repo has been soft-deleted. The `WHERE EXISTS` is
    /// part of the insert rather than a preceding SELECT because the
    /// scheduler reads its repo row long before it gets here: a delete
    /// landing in between would pass any check-then-insert, and the
    /// foreign key cannot catch it either — a flagged row is still
    /// physically present. The job that slips through is worse than a lost
    /// cycle: `forget_deleted_repo` is a plain DELETE, so a single
    /// referencing job wedges the repo permanently half-deleted —
    /// invisible to every read path, unreapable, still accruing work.
    pub async fn create_job(
        &self,
        kind: JobKind,
        repo_id: i64,
        finding_id: Option<i64>,
        cap_tokens: i64,
        state: JobState,
    ) -> Result<i64, StoreWriteError> {
        let now = now_ms();
        let result = sqlx::query!(
            "INSERT INTO jobs (kind, repo_id, finding_id, cap_tokens, state, started_at) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6 \
             WHERE EXISTS (SELECT 1 FROM repos WHERE id = ?2 AND deleted_at IS NULL)",
            kind,
            repo_id,
            finding_id,
            cap_tokens,
            state,
            now
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreWriteError::Refused(format!(
                "repo {repo_id} is deleted -- cannot start a {kind} job"
            )));
        }
        Ok(result.last_insert_rowid())
    }

    /// Record a completed job (all outcome fields set at once).
    pub async fn complete_job(
        &self,
        job_id: i64,
        state: JobState,
        tokens_new: i64,
        calls: i64,
        exit_code: Option<i64>,
        killed_reason: Option<&str>,
        session_file: Option<&str>,
        notes: Option<&str>,
        model: Option<&str>,
        usage_delta: Option<f64>,
        finished_at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, tokens_new = ?2, calls = ?3, \
             exit_code = ?4, killed_reason = ?5, session_file = ?6, notes = ?7, \
             model = ?8, usage_delta = ?9, finished_at = ?10 \
             WHERE id = ?11",
            state,
            tokens_new,
            calls,
            exit_code,
            killed_reason,
            session_file,
            notes,
            model,
            usage_delta,
            finished_at,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark an orphaned running job as killed during reconciliation.
    pub async fn orphan_job(&self, job_id: i64, notes: &str, finished_at: i64) -> sqlx::Result<()> {
        let killed = JobState::Killed;
        sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, killed_reason = 'orphaned', \
             notes = ?2, finished_at = ?3 WHERE id = ?4",
            killed,
            notes,
            finished_at,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Downgrade an already-recorded job to failed (e.g. post-run push failure).
    pub async fn fail_job(&self, job_id: i64, notes: &str) -> sqlx::Result<()> {
        let failed = JobState::Failed;
        sqlx::query!(
            "UPDATE jobs SET state = ?1, notes = ?2 WHERE id = ?3",
            failed,
            notes,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE repos SET `last_hunt_sha` = ?, `last_hunt_at` = now WHERE id = ?.
    pub async fn set_last_hunt(&self, repo_id: i64, sha: &str) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_hunt_sha = ?1, last_hunt_at = ?2 WHERE id = ?3",
            sha,
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE repos SET `last_full_hunt_at` = now WHERE id = ?.
    pub async fn set_last_full_hunt(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_full_hunt_at = ?1 WHERE id = ?2",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Targeted query for `anticipated_tokens`: is there a warm (non-denied,
    /// finished within cutoff) job for this exact (repo, kind)?
    pub async fn has_warm_job(
        &self,
        repo_id: i64,
        kind: &str,
        cutoff_ms: i64,
    ) -> sqlx::Result<bool> {
        let row = sqlx::query_scalar!(
            r#"SELECT 1 AS "x!: i64" FROM jobs
               WHERE repo_id = ?1 AND kind = ?2 AND finished_at > ?3
               AND state != 'denied' LIMIT 1"#,
            repo_id,
            kind,
            cutoff_ms
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Targeted query for `anticipated_tokens`: all `tokens_new` values for
    /// a given kind, sorted ascending (includes denied rows — Python parity).
    pub async fn kind_token_history(&self, kind: &str) -> sqlx::Result<Vec<i64>> {
        let rows = sqlx::query_scalar!(
            r#"SELECT tokens_new AS "tokens_new!: i64" FROM jobs
               WHERE kind = ?1 AND tokens_new IS NOT NULL
               ORDER BY tokens_new ASC"#,
            kind
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// UPSERT INTO `scheduler_state` (id=1, state, detail, `next_wake_at`, `updated_at`).
    pub async fn set_scheduler_state(
        &self,
        state: &str,
        detail: &str,
        next_wake_at: Option<i64>,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO scheduler_state (id, state, detail, next_wake_at, updated_at) \
             VALUES (1, ?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET state=excluded.state, detail=excluded.detail, \
             next_wake_at=excluded.next_wake_at, updated_at=excluded.updated_at",
            state,
            detail,
            next_wake_at,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Findings with suppressed statuses for a repo+type (from `FindingStatus::is_suppressed`).
    pub async fn suppressions(
        &self,
        repo_id: i64,
        finding_type: &str,
    ) -> sqlx::Result<Vec<Finding>> {
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override, fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE repo_id = ?1 AND type = ?2
              AND status IN (?3, ?4)
            ORDER BY id
            "#,
            repo_id,
            finding_type,
            s1,
            s2
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Non-suppressed findings for a repo+type (novelty comparison).
    ///
    /// Python selected `status IN ACTIVE_STATUSES` (`types.ACTIVE_STATUSES`), which
    /// is seven statuses — the five in-flight ones *plus* `merged` and
    /// `note`. Excluding the two suppressed statuses from the nine is the
    /// same set, stated as its complement: a finding already merged or
    /// noted is still knowledge a hunt should not rediscover as novel.
    /// Read it as "everything except the verdicts that mean forget this",
    /// not as "everything still in flight".
    pub async fn known_active(
        &self,
        repo_id: i64,
        finding_type: &str,
    ) -> sqlx::Result<Vec<Finding>> {
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override, fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE repo_id = ?1 AND type = ?2
              AND status NOT IN (?3, ?4)
            ORDER BY id
            "#,
            repo_id,
            finding_type,
            s1,
            s2
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Insert a finding unless one with the same (type, fingerprint) already
    /// exists. Returns (id, `was_inserted`).
    ///
    /// Deliberately *not* `INSERT OR IGNORE`: the lookup and the INSERT are
    /// separate statements, so two callers racing on the same fingerprint
    /// would have the second fail the UNIQUE constraint rather than be
    /// ignored. Only the single scheduler loop ingests, so no second caller
    /// exists to open that window.
    /// Insert a finding, or report the existing one with this fingerprint.
    ///
    /// `found_by_job` is stored only on a genuine insert: a duplicate
    /// belongs to the job that first turned it up, not the latest one to
    /// rediscover it.
    pub async fn upsert_finding(
        &self,
        repo_id: i64,
        row: &FindingInsert,
        finding_type: &str,
        found_by_job: Option<i64>,
    ) -> sqlx::Result<(i64, bool)> {
        // Check if already exists
        let fingerprint = &row.fingerprint;
        let existing = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM findings WHERE type = ?1 AND fingerprint = ?2"#,
            finding_type,
            fingerprint
        )
        .fetch_optional(&self.pool)
        .await?;
        if let Some(id) = existing {
            return Ok((id, false));
        }
        let now = now_ms();
        let file = &row.file;
        let symbol = row.symbol.as_deref();
        let line = row.line;
        let bug_class = row.bug_class.as_deref();
        let severity = &row.severity;
        let confidence = row.confidence;
        let summary = &row.summary;
        let detail = row.detail.as_deref();
        let evidence_plan = row.evidence_plan.as_deref();
        let introduced_by = row.introduced_by.as_deref();
        let ecosystem = row.ecosystem.as_deref();
        let package = row.package.as_deref();
        let current_version = row.current_version.as_deref();
        let latest_version = row.latest_version.as_deref();
        let update_type = row.update_type.as_deref();
        let security_advisory = row.security_advisory.as_deref();
        let missing_tests_ref = row.missing_tests.as_deref();
        let test_file = row.test_file.as_deref();
        let smell_type = row.smell_type.as_deref();
        let suggested_refactor = row.suggested_refactor.as_deref();
        let modernization_class = row.modernization_class.as_deref();
        let current_approach = row.current_approach.as_deref();
        let proposed_approach = row.proposed_approach.as_deref();
        let standard_section = row.standard_section.as_deref();
        let new_status = FindingStatus::New;
        let result = sqlx::query!(
            "INSERT INTO findings (type, repo_id, fingerprint, file, symbol, line, bug_class, \
             severity, confidence, summary, detail, evidence_plan, introduced_by, \
             ecosystem, package, current_version, latest_version, update_type, security_advisory, \
             missing_tests, test_file, smell_type, suggested_refactor, \
             modernization_class, current_approach, proposed_approach, standard_section, \
             status, created_at, updated_at, found_by_job) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27, ?28, ?29, ?30, ?31)",
            finding_type, repo_id, fingerprint, file, symbol, line, bug_class,
            severity, confidence, summary, detail, evidence_plan, introduced_by,
            ecosystem, package, current_version, latest_version, update_type, security_advisory,
            missing_tests_ref, test_file, smell_type, suggested_refactor,
            modernization_class, current_approach, proposed_approach, standard_section,
            new_status, now, now, found_by_job
        )
        .execute(&self.pool)
        .await?;
        Ok((result.last_insert_rowid(), true))
    }

    /// Mark a PR as merged (UPSERT).
    pub async fn mark_pr_merged(
        &self,
        finding_id: i64,
        pr_number: i64,
        synced_at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number, state, needs_attention, synced_at) \
             VALUES (?1, ?2, 'MERGED', NULL, ?3) \
             ON CONFLICT(finding_id) DO UPDATE SET \
             pr_number = excluded.pr_number, state = 'MERGED', \
             needs_attention = NULL, synced_at = excluded.synced_at",
            finding_id,
            pr_number,
            synced_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a PR as closed (UPSERT).
    pub async fn mark_pr_closed(
        &self,
        finding_id: i64,
        pr_number: i64,
        synced_at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number, state, needs_attention, synced_at) \
             VALUES (?1, ?2, 'CLOSED', NULL, ?3) \
             ON CONFLICT(finding_id) DO UPDATE SET \
             pr_number = excluded.pr_number, state = 'CLOSED', \
             needs_attention = NULL, synced_at = excluded.synced_at",
            finding_id,
            pr_number,
            synced_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Full sync of an open PR.  The main UPSERT sets all always-present
    /// columns; two conditional queries handle `attention_since` and
    /// clearing addressed state — three compile-time checked queries
    /// instead of one dynamic one.
    ///
    /// All three share one transaction because the conditional UPDATEs are
    /// decided from the state the UPSERT writes: `attention_since` is only
    /// stamped on the sync where the attention reason *changes*, so if the
    /// UPSERT committed the new `attention_fingerprint` and the UPDATE then
    /// failed, every later sync would see an unchanged reason and never
    /// stamp it — leaving `list_attention` ordering and the displayed
    /// attention age permanently wrong.  `clear_addressed` has the same
    /// shape.  A plain (deferred) `begin` suffices: the first statement is
    /// a write, so the lock is taken before anything is read back.
    pub async fn sync_pr_open(&self, finding_id: i64, d: &SyncPrData) -> sqlx::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number, state, mergeable, checks, \
             head_ref, head_sha, last_activity_at, last_engaged_activity_at, \
             needs_attention, attention_fingerprint, synced_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(finding_id) DO UPDATE SET \
             pr_number = excluded.pr_number, \
             state = excluded.state, \
             mergeable = excluded.mergeable, \
             checks = excluded.checks, \
             head_ref = excluded.head_ref, \
             head_sha = excluded.head_sha, \
             last_activity_at = excluded.last_activity_at, \
             last_engaged_activity_at = excluded.last_engaged_activity_at, \
             needs_attention = excluded.needs_attention, \
             attention_fingerprint = excluded.attention_fingerprint, \
             synced_at = excluded.synced_at",
            finding_id,
            d.pr_number,
            d.state,
            d.mergeable,
            d.checks,
            d.head_ref,
            d.head_sha,
            d.last_activity_at,
            d.last_engaged_activity_at,
            d.needs_attention,
            d.attention_fingerprint,
            d.synced_at
        )
        .execute(&mut *tx)
        .await?;
        // Conditional: set attention_since when the reason changed.
        if let Some(since) = d.attention_since {
            sqlx::query!(
                "UPDATE pr_state SET attention_since = ?1 WHERE finding_id = ?2",
                since,
                finding_id
            )
            .execute(&mut *tx)
            .await?;
        }
        // Conditional: clear addressed state when the static snapshot changed.
        if d.clear_addressed {
            sqlx::query!(
                "UPDATE pr_state SET addressed_fingerprint = NULL, \
                 addressed_head_sha = NULL WHERE finding_id = ?1",
                finding_id
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Update engagement watermark and addressed state after a successful
    /// engage cycle.  `addressed_fingerprint` / `addressed_head_sha` are
    /// nullable: pass None to clear (pushed) or Some to set (replied-only).
    pub async fn mark_pr_engaged(
        &self,
        finding_id: i64,
        last_engaged_activity_at: i64,
        synced_at: i64,
        addressed_fingerprint: Option<&str>,
        addressed_head_sha: Option<&str>,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_state SET last_engaged_activity_at = ?1, synced_at = ?2, \
             addressed_fingerprint = ?3, addressed_head_sha = ?4 \
             WHERE finding_id = ?5",
            last_engaged_activity_at,
            synced_at,
            addressed_fingerprint,
            addressed_head_sha,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a merged PR as harvested.
    pub async fn mark_pr_harvested(&self, finding_id: i64, harvested_at: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_state SET harvested_at = ?1 WHERE finding_id = ?2",
            harvested_at,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Self-heal a missing `pr_number` (UPSERT — row may not exist yet).
    pub async fn set_pr_number(&self, finding_id: i64, pr_number: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number) VALUES (?1, ?2) \
             ON CONFLICT(finding_id) DO UPDATE SET pr_number = excluded.pr_number",
            finding_id,
            pr_number
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Findings at `pr_open` with `needs_attention` set, sorted by `attention_since`.
    pub async fn list_attention(&self) -> sqlx::Result<Vec<Finding>> {
        let pr_open = FindingStatus::PrOpen;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT f.id, f.type AS "kind: FindingType", f.repo_id, f.fingerprint, f.file, f.symbol, f.line,
                   f.severity AS "severity: Severity", f.confidence, f.summary, f.detail, f.status AS "status: FindingStatus", f.pr_url,
                   f.created_at, f.updated_at, f.bug_class AS "bug_class: BugClass", f.evidence_plan,
                   f.introduced_by, f.rung_achieved, f.verdict_reason,
                   f.budget_override, f.fix_attempts, f.last_fix_failure,
                   f.recheck_attempts, f.last_recheck_failure, f.ecosystem, f.package,
                   f.current_version, f.latest_version, f.update_type,
                   f.security_advisory, f.missing_tests, f.test_file, f.smell_type,
                   f.suggested_refactor, f.modernization_class, f.current_approach,
                   f.proposed_approach, f.standard_section
            FROM findings f
            JOIN pr_state p ON p.finding_id = f.id
            WHERE f.status = ?1 AND p.needs_attention IS NOT NULL
            ORDER BY COALESCE(p.attention_since, p.synced_at) ASC
            "#,
            pr_open
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Merged findings pending harvest review.
    pub async fn list_pending_harvest(&self) -> sqlx::Result<Vec<Finding>> {
        let merged = FindingStatus::Merged;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT f.id, f.type AS "kind: FindingType", f.repo_id, f.fingerprint, f.file, f.symbol, f.line,
                   f.severity AS "severity: Severity", f.confidence, f.summary, f.detail, f.status AS "status: FindingStatus", f.pr_url,
                   f.created_at, f.updated_at, f.bug_class AS "bug_class: BugClass", f.evidence_plan,
                   f.introduced_by, f.rung_achieved, f.verdict_reason,
                   f.budget_override, f.fix_attempts, f.last_fix_failure,
                   f.recheck_attempts, f.last_recheck_failure, f.ecosystem, f.package,
                   f.current_version, f.latest_version, f.update_type,
                   f.security_advisory, f.missing_tests, f.test_file, f.smell_type,
                   f.suggested_refactor, f.modernization_class, f.current_approach,
                   f.proposed_approach, f.standard_section
            FROM findings f
            JOIN pr_state p ON p.finding_id = f.id
            WHERE f.status = ?1 AND p.harvested_at IS NULL
            ORDER BY p.synced_at ASC
            "#,
            merged
        )
        .fetch_all(&self.pool)
        .await
    }

    /// UPDATE finding's analysis fields (summary, detail, etc.) after a
    /// worker recheck.  Uses COALESCE to skip NULL fields.
    pub async fn update_finding_analysis(
        &self,
        finding_id: i64,
        fields: &FindingAnalysisUpdate,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        let summary = fields.summary.as_deref();
        let detail = fields.detail.as_deref();
        let severity = fields.severity.as_deref();
        sqlx::query!(
            "UPDATE findings SET \
             updated_at = ?1, \
             summary = COALESCE(?2, summary), \
             detail = COALESCE(?3, detail), \
             confidence = COALESCE(?4, confidence), \
             severity = COALESCE(?5, severity) \
             WHERE id = ?6",
            now,
            summary,
            detail,
            fields.confidence,
            severity,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Recover orphaned jobs (state=running) and findings (status=fixing)
    /// from a crashed prior process.
    pub async fn reconcile_orphaned_jobs(&self) -> sqlx::Result<(Vec<Finding>, Vec<Job>)> {
        // Findings stuck at 'fixing' -> queued
        let stuck_findings = self
            .list_findings(&FindingFilter {
                status: Some(FindingStatus::Fixing),
                ..FindingFilter::default()
            })
            .await?;
        for f in &stuck_findings {
            self.set_finding_status(f.id, FindingStatus::Queued).await?;
        }
        // Jobs stuck at 'running' -> killed/orphaned
        let mut orphaned_jobs = Vec::new();
        for j in self.list_running_jobs().await? {
            let now = now_ms();
            self.orphan_job(
                j.id,
                "reconciled at cycle startup -- prior process died mid-job",
                now,
            )
            .await?;
            orphaned_jobs.push(j);
        }
        Ok((stuck_findings, orphaned_jobs))
    }

    /// Record a fix attempt fingerprint for retry-limiting.
    pub async fn record_fix_attempt(
        &self,
        finding_id: i64,
        failure_fingerprint: &str,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        // Streak tracking: same failure -> increment; different -> reset to 1
        let finding = self.get_finding(finding_id).await?;
        let (prev_failure, prev_attempts) = match &finding {
            Some(f) => (f.last_fix_failure.as_deref(), f.fix_attempts),
            None => (None, 0),
        };
        let attempts = if Some(failure_fingerprint) == prev_failure {
            prev_attempts + 1
        } else {
            1
        };
        sqlx::query!(
            "UPDATE findings SET fix_attempts = ?1, last_fix_failure = ?2, updated_at = ?3 WHERE id = ?4",
            attempts, failure_fingerprint, now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(attempts)
    }
    pub async fn clear_fix_attempts(&self, finding_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET fix_attempts = 0, last_fix_failure = NULL, updated_at = ?1 WHERE id = ?2",
            now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn record_recheck_attempt(
        &self,
        finding_id: i64,
        failure_fingerprint: &str,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        let finding = self.get_finding(finding_id).await?;
        let (prev_failure, prev_attempts) = match &finding {
            Some(f) => (f.last_recheck_failure.as_deref(), f.recheck_attempts),
            None => (None, 0),
        };
        let attempts = if Some(failure_fingerprint) == prev_failure {
            prev_attempts + 1
        } else {
            1
        };
        sqlx::query!(
            "UPDATE findings SET recheck_attempts = ?1, last_recheck_failure = ?2, updated_at = ?3 WHERE id = ?4",
            attempts, failure_fingerprint, now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(attempts)
    }
    pub async fn clear_recheck_attempts(&self, finding_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET recheck_attempts = 0, last_recheck_failure = NULL, updated_at = ?1 WHERE id = ?2",
            now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn record_harvest_attempt(
        &self,
        finding_id: i64,
        failure_fingerprint: &str,
    ) -> sqlx::Result<i64> {
        // harvest_attempts are stored in pr_state, not findings
        let ps = self.get_pr_state(finding_id).await?;
        let (prev_failure, prev_attempts) = match &ps {
            Some(p) => (p.last_harvest_failure.as_deref(), p.harvest_attempts),
            None => (None, 0),
        };
        let attempts = if Some(failure_fingerprint) == prev_failure {
            prev_attempts + 1
        } else {
            1
        };
        sqlx::query!(
            "UPDATE pr_state SET harvest_attempts = ?1, last_harvest_failure = ?2 \
             WHERE finding_id = ?3",
            attempts,
            failure_fingerprint,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(attempts)
    }
    pub async fn clear_harvest_attempts(&self, finding_id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_state SET harvest_attempts = 0, last_harvest_failure = NULL \
             WHERE finding_id = ?1",
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Context manager equivalent: set status, run body, if status
    /// unchanged on exit set fallback. Returns a guard that checks on
    /// drop. In Rust: explicit start/finish calls instead.
    pub async fn set_in_progress(
        &self,
        finding_id: i64,
        status: FindingStatus,
    ) -> sqlx::Result<()> {
        self.set_finding_status(finding_id, status).await
    }
    /// If finding is still at `expected_status`, reset to `fallback`.
    pub async fn finalize_in_progress(
        &self,
        finding_id: i64,
        expected_status: FindingStatus,
        fallback: FindingStatus,
    ) -> sqlx::Result<()> {
        if let Some(f) = self.get_finding(finding_id).await?
            && f.status == expected_status
        {
            self.set_finding_status(finding_id, fallback).await?;
        }
        Ok(())
    }

    // -- stats -----------------------------------------------------------------

    pub async fn stats_totals(&self) -> sqlx::Result<StatsTotals> {
        sqlx::query_as!(
            StatsTotals,
            r#"
            SELECT COUNT(*) AS "jobs!: i64",
                   SUM(tokens_new) AS "total_tokens: i64",
                   SUM(calls) AS "total_calls: i64",
                   SUM(usage_delta) AS "total_usage_delta: f64",
                   SUM(CASE WHEN state = 'done' THEN 1 ELSE 0 END) AS "done: i64",
                   SUM(CASE WHEN state = 'denied' THEN 1 ELSE 0 END) AS "denied: i64"
            FROM jobs
            "#
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn stats_by_kind(&self) -> sqlx::Result<Vec<StatsByKind>> {
        sqlx::query_as!(
            StatsByKind,
            r#"
            SELECT kind AS "kind!: JobKind", COUNT(*) AS "jobs!: i64",
                   SUM(CASE WHEN state = 'done' THEN 1 ELSE 0 END) AS "done: i64",
                   SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END) AS "failed: i64",
                   SUM(CASE WHEN state = 'killed' THEN 1 ELSE 0 END) AS "killed: i64",
                   SUM(CASE WHEN state = 'denied' THEN 1 ELSE 0 END) AS "denied: i64",
                   SUM(tokens_new) AS "total_tokens: i64",
                   SUM(calls) AS "total_calls: i64",
                   AVG(tokens_new) AS "avg_tokens: f64",
                   SUM(usage_delta) AS "total_usage_delta: f64",
                   GROUP_CONCAT(DISTINCT model) AS "models: String"
            FROM jobs
            GROUP BY kind
            ORDER BY kind
            "#
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn stats_by_finding(&self) -> sqlx::Result<Vec<StatsByFinding>> {
        // ORDER BY repeats the SUM expression: the Python source orders by the
        // `total_tokens` alias, but our alias carries a sqlx type override, so
        // the bare name would not resolve. Identical semantics (NULLs last on
        // DESC in SQLite).
        sqlx::query_as!(
            StatsByFinding,
            r#"
            SELECT j.finding_id AS "finding_id!: i64",
                   f.fingerprint AS "fingerprint!", f.status AS "status!",
                   f.severity AS "severity!", COUNT(*) AS "jobs!: i64",
                   SUM(j.tokens_new) AS "total_tokens: i64",
                   SUM(j.calls) AS "total_calls: i64",
                   SUM(j.usage_delta) AS "total_usage_delta: f64"
            FROM jobs j
            JOIN findings f ON f.id = j.finding_id
            WHERE j.finding_id IS NOT NULL
            GROUP BY j.finding_id
            ORDER BY SUM(j.tokens_new) DESC
            "#
        )
        .fetch_all(&self.pool)
        .await
    }
}

/// `SpendLedger` over the same pool (BACKEND-CONTRACT.md §1.6 — exact SQL
/// there; Python's `ThreadLocalLedger` dissolves under the pool).
#[async_trait::async_trait]
impl crate::backend::SpendLedger for Store {
    async fn running_estimate(&self) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            r#"SELECT COALESCE(SUM(cap_tokens), 0) AS "total!: i64"
               FROM jobs WHERE state = 'running'"#
        )
        .fetch_one(&self.pool)
        .await
    }

    async fn finished_since(&self, ts_ms: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            // `tokens_new IS NOT NULL` does not change the sum — SUM skips
            // NULLs — but it is what makes the partial `jobs_finished_at`
            // index eligible. Without it SQLite full-scans `jobs` on the
            // endpoint the UI polls every 5s.
            r#"SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64"
               FROM jobs
               WHERE state != 'running'
                 AND finished_at > ?1
                 AND tokens_new IS NOT NULL"#,
            ts_ms
        )
        .fetch_one(&self.pool)
        .await
    }

    async fn finished_between(&self, start_ms: i64, end_ms: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            // Same partial-index predicate as `finished_since`.
            r#"SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64"
               FROM jobs
               WHERE state != 'running'
                 AND finished_at > ?1
                 AND finished_at <= ?2
                 AND tokens_new IS NOT NULL"#,
            start_ms,
            end_ms
        )
        .fetch_one(&self.pool)
        .await
    }

    async fn log_window_observation(
        &self,
        limit_id: &str,
        used_fraction: Option<f64>,
        status: Option<&str>,
        resets_at: Option<i64>,
        age_s: f64,
    ) -> sqlx::Result<()> {
        let observed_at = now_ms();
        // int(age_s): truncation toward zero, matching Python (`Store.log_window_observation`).
        let source_age_s = age_s as i64;
        sqlx::query!(
            "INSERT INTO window_log \
             (observed_at, limit_id, used_fraction, status, resets_at, source_age_s) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            observed_at,
            limit_id,
            used_fraction,
            status,
            resets_at,
            source_age_s
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn last_window_observation(
        &self,
        limit_id: &str,
        resets_at: i64,
    ) -> sqlx::Result<Option<(i64, f64)>> {
        let lo = resets_at - 5_000;
        let hi = resets_at + 5_000;
        let row = sqlx::query!(
            r#"SELECT observed_at AS "observed_at!: i64",
                      used_fraction AS "used_fraction: f64"
               FROM window_log
               WHERE limit_id = ?1 AND resets_at BETWEEN ?2 AND ?3
               ORDER BY observed_at DESC
               LIMIT 1"#,
            limit_id,
            lo,
            hi
        )
        .fetch_optional(&self.pool)
        .await?;
        // None when no row OR the newest row's used_fraction is NULL
        // (`Store.last_window_observation`).
        Ok(row.and_then(|r| r.used_fraction.map(|f| (r.observed_at, f))))
    }

    async fn record_calibration_sample(
        &self,
        limit_id: &str,
        window_resets_at: Option<i64>,
        used_fraction_delta: f64,
        hunter_tokens: i64,
    ) -> sqlx::Result<()> {
        let observed_at = now_ms();
        sqlx::query!(
            "INSERT INTO calibration_samples \
             (observed_at, limit_id, window_resets_at, used_fraction_delta, hunter_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            observed_at,
            limit_id,
            window_resets_at,
            used_fraction_delta,
            hunter_tokens
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn estimate_capacity(&self, limit_id: &str) -> sqlx::Result<Option<f64>> {
        // Newest completed cycles considered (store.py sample_limit=200).
        const SAMPLE_LIMIT: i64 = 200;
        let period_ms: i64 = match limit_id {
            "anthropic:5h" => 18_000_000,
            "anthropic:7d" => 604_800_000,
            // Per-model-class / unknown lids have no known period.
            _ => return Ok(None),
        };
        let now = now_ms();
        // One statement, not one per cycle. Previously this fetched up to
        // SAMPLE_LIMIT cycles and then ran a SUM over `jobs` for each of
        // them; with 200 cycles that is 200 sequential round trips, and
        // `status_html` plus `decide` call this up to six times per
        // GET /api/summary — the endpoint the UI polls every 5s.
        //
        // The cycle set still dedupes resets_at into 10 s buckets and
        // takes the newest SAMPLE_LIMIT, and the per-cycle spend is still
        // a half-open (start, resets] window; only the number of
        // statements changes.
        let best = sqlx::query_scalar!(
            r#"
            WITH cycles AS (
                SELECT MIN(resets_at) AS resets
                FROM window_log
                WHERE limit_id = ?1 AND resets_at < ?2
                GROUP BY CAST(resets_at / 10000 AS INT)
                ORDER BY CAST(resets_at / 10000 AS INT) DESC
                LIMIT ?3
            )
            SELECT COALESCE(MAX(spent), 0) AS "best!: i64"
            FROM (
                SELECT (
                    SELECT COALESCE(SUM(j.tokens_new), 0)
                    FROM jobs j
                    WHERE j.state NOT IN ('denied', 'running')
                      AND j.tokens_new IS NOT NULL
                      AND j.finished_at > c.resets - ?4
                      AND j.finished_at <= c.resets
                ) AS spent
                FROM cycles c
                WHERE c.resets IS NOT NULL
            )
            "#,
            limit_id,
            now,
            SAMPLE_LIMIT,
            period_ms
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((best > 0).then_some(best as f64))
    }
}
