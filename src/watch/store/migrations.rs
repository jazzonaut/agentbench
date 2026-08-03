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

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: super::schema::CREATE_V1,
    },
    Migration {
        version: 2,
        sql: super::schema::ALTER_V2,
    },
    Migration {
        version: 3,
        sql: super::schema::ALTER_V3,
    },
];

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

    /// Databases written by the phase-1 build exist on real machines, so v2 has to reach them.
    #[test]
    fn a_v1_database_gains_the_session_turn_corrections() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::watch::store::schema::CREATE_V1)
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        migrate(&mut conn).unwrap();

        let column = |name: &str| -> u32 {
            conn.query_row(
                "SELECT count(*) FROM pragma_table_info('session_turns') WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(column("first_response_ms"), 1, "the honest name");
        assert_eq!(column("ttft_ms"), 0, "the misleading one is gone");
        assert_eq!(column("session_id"), 1);
        assert_eq!(column("request_id"), 1);
    }

    /// The rollup target has to reach every series the dashboard advertises, on old databases too.
    #[test]
    fn a_v2_database_gains_the_rollup_columns_retention_needs() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::watch::store::schema::CREATE_V1)
            .unwrap();
        conn.execute_batch(crate::watch::store::schema::ALTER_V2)
            .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();

        migrate(&mut conn).unwrap();

        for name in ["process_count_avg", "agent_rss_max"] {
            let found: u32 = conn
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('samples_1m') WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "samples_1m.{name} missing");
        }
    }

    /// The guarantee that keeps token counts honest when an import resumes mid-request.
    #[test]
    fn one_request_can_only_be_recorded_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES ('m', 'm', 'os', '1', 'x86_64', 'cpu', 1, 0, 0, 0)",
            [],
        )
        .unwrap();
        let insert = |uuid: &str| {
            conn.execute(
                "INSERT OR IGNORE INTO session_turns (uuid, machine_id, ts, request_id,
                     output_tokens) VALUES (?1, 'm', 1, 'req_1', 400)",
                [uuid],
            )
        };
        insert("first-row").unwrap();
        insert("a-different-row-of-the-same-request").unwrap();

        let (turns, tokens): (i64, i64) = conn
            .query_row(
                "SELECT count(*), coalesce(sum(output_tokens), 0) FROM session_turns",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(turns, 1);
        assert_eq!(tokens, 400, "tokens must not be counted twice");
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
