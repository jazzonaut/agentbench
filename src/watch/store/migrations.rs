//! Forward-only schema migrations keyed on `PRAGMA user_version`.
//!
//! Distinct from [`crate::SCHEMA_VERSION`], which versions exported JSON reports. Adding a migration
//! means appending to [`MIGRATIONS`]; existing entries are immutable once released.

use anyhow::{Result, bail};
use rusqlite::Connection;

/// One forward migration from `version - 1` to `version`.
struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: super::schema::CREATE_V1,
}];

/// Version this build expects after migrating.
pub fn target_version() -> u32 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

/// Bring `conn` up to [`target_version`], applying each step in its own transaction.
///
/// A database from a *newer* build is refused rather than downgraded: silently operating on a schema
/// we do not understand would corrupt history that cannot be regenerated.
pub fn migrate(conn: &mut Connection) -> Result<u32> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let target = target_version();
    if current > target {
        bail!(
            "database schema version {current} is newer than this build understands ({target}); \
             upgrade AgentBench or point --data-dir at a different directory"
        );
    }
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        // PRAGMA user_version does not accept a bound parameter.
        tx.pragma_update(None, "user_version", migration.version)?;
        tx.commit()?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_a_fresh_database_reaches_the_target_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), target_version());
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, target_version());
    }

    #[test]
    fn migrating_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let tables: u32 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='samples'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "a second migrate must not recreate tables");
    }

    #[test]
    fn every_documented_table_exists_after_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        for table in [
            "machines",
            "samples",
            "samples_1m",
            "probe_runs",
            "probe_metrics",
            "session_turns",
            "session_tools",
            "run_markers",
            "tool_versions",
            "import_watermark",
            "events",
        ] {
            let found: u32 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} missing");
        }
    }

    #[test]
    fn a_newer_database_is_refused_rather_than_downgraded() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", target_version() + 5)
            .unwrap();
        let error = migrate(&mut conn).unwrap_err().to_string();
        assert!(error.contains("newer than this build"), "{error}");
    }
}
