// Build script — runs at compile time, not in production.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! Create dev.db with the final schema for `sqlx::query!` validation.
//! Applies migrations sequentially via sqlite3. Migration 002 does
//! a table rebuild (RENAME → CREATE → INSERT → DROP) which is atomic
//! within a single sqlite3 session.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=migrations");

    let dev_db = Path::new("dev.db");
    let _ = std::fs::remove_file(dev_db);

    let mut migrations: Vec<_> = std::fs::read_dir("migrations")
        .expect("migrations/ directory must exist")
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "sql"))
        .collect();
    migrations.sort_by_key(std::fs::DirEntry::file_name);

    for migration in &migrations {
        let sql = std::fs::read_to_string(migration.path())
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", migration.path().display()));
        // -bail: without it sqlite3 keeps executing after a failed
        // statement, so a broken migration leaves dev.db partly applied.
        let status = Command::new("sqlite3")
            .arg("-bail")
            .arg("dev.db")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(sql.as_bytes())?;
                child.wait()
            })
            .unwrap_or_else(|e| panic!("sqlite3 failed on {}: {e}", migration.path().display()));
        assert!(
            status.success(),
            "sqlite3 failed on {}",
            migration.path().display()
        );
        stamp(dev_db, &migration.path());
    }
}

/// Record a migration as applied, exactly as sqlx would.
///
/// Without this, dev.db carries the full schema but no migration
/// history, so every test that copies it replays all migrations against
/// a database that already has their effects. That happens to work for
/// `ALTER TABLE ... ADD COLUMN` on `findings` only because migrations
/// 002 and 003 rebuild that table and drop the columns again; the same
/// statement against any other table fails with "duplicate column name"
/// — a trap that springs on whoever writes the next one, far from its
/// cause.
///
/// The checksum is SHA-384 over the migration's bytes, which is what
/// sqlx verifies on startup: get it wrong and every connect fails loudly
/// with "previously applied but has been modified", never silently.
fn stamp(dev_db: &Path, migration: &Path) {
    let name = migration
        .file_stem()
        .expect("migration file stem")
        .to_string_lossy()
        .into_owned();
    let (version, rest) = name.split_once('_').expect("NNN_description.sql");
    let description = rest.replace('_', " ");
    let digest = Command::new("sha384sum")
        .arg(migration)
        .output()
        .expect("sha384sum must be available to stamp dev.db");
    assert!(digest.status.success(), "sha384sum failed on {name}");
    let hex = String::from_utf8_lossy(&digest.stdout)
        .split_whitespace()
        .next()
        .expect("sha384sum output")
        .to_owned();
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
             version BIGINT PRIMARY KEY,
             description TEXT NOT NULL,
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
             success BOOLEAN NOT NULL,
             checksum BLOB NOT NULL,
             execution_time BIGINT NOT NULL
         );
         INSERT OR REPLACE INTO _sqlx_migrations
             (version, description, success, checksum, execution_time)
         VALUES ({version}, '{description}', 1, X'{hex}', 0);"
    );
    let status = Command::new("sqlite3")
        .arg("-bail")
        .arg(dev_db)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(sql.as_bytes())?;
            child.wait()
        })
        .unwrap_or_else(|e| panic!("cannot stamp {name}: {e}"));
    assert!(status.success(), "cannot stamp {name}");
}
