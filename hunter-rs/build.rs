// Build script — runs at compile time, not in production.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! Create dev.db with the final schema for sqlx::query! validation.
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
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "sql"))
        .collect();
    migrations.sort_by_key(|e| e.file_name());

    for migration in &migrations {
        let sql = std::fs::read_to_string(migration.path())
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", migration.path().display()));
        let status = Command::new("sqlite3")
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
    }
}
