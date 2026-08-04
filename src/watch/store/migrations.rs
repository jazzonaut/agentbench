//! Forward-only schema migrations keyed on `PRAGMA user_version`.
//!
//! Distinct from [`crate::SCHEMA_VERSION`], which versions exported JSON reports. Adding a migration
//! means appending to [`MIGRATIONS`]; existing entries are immutable once released.
//!
//! There is one entry, and that is deliberate rather than the state of a young project: five of them
//! existed and were collapsed before 0.7.0, because no release had shipped any of them and they
//! described an upgrade from a database that exists nowhere. See [`super::schema`].

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
        // Reachable two ways, and the message has to serve both. A genuinely newer build is the case
        // this guard was written for. The other is a database written by 0.6.x, whose schema counter had
        // reached 5 before the migrations were collapsed — so on the machines that were collecting
        // during development, "newer" means "older", and the remedy is to move the file aside rather
        // than to install anything.
        bail!(
            "database schema version {current} is not one this build understands ({target}); \
             either it was written by a newer AgentBench, or it predates the 0.7.0 schema reset — \
             move the file aside or point --data-dir somewhere else"
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
            "probe_processes",
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

    /// Every passive series the dashboard can chart needs a rollup column, or its history stops dead at
    /// the retention boundary while the chart beside it keeps going.
    ///
    /// Asserted against the columns rather than against a list in prose, because the failure is silent:
    /// nothing writes to `samples_1m` until a fortnight has passed on a real machine.
    #[test]
    fn every_raw_sample_column_has_a_rollup_counterpart() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let column = |table: &str, name: &str| -> u32 {
            conn.query_row(
                &format!("SELECT count(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                [name],
                |row| row.get(0),
            )
            .unwrap()
        };
        for (raw, rolled) in [
            ("cpu_percent", "cpu_avg"),
            ("used_memory", "used_memory_avg"),
            ("used_swap", "used_swap_max"),
            ("process_count", "process_count_avg"),
            ("scanner_cpu", "scanner_cpu_max"),
            ("agent_cpu", "agent_cpu_max"),
            ("agent_rss", "agent_rss_max"),
            ("agent_write_bytes_s", "agent_write_bytes_s_max"),
            ("scanner_write_bytes_s", "scanner_write_bytes_s_max"),
        ] {
            assert_eq!(column("samples", raw), 1, "samples.{raw} missing");
            assert_eq!(column("samples_1m", rolled), 1, "samples_1m.{rolled} missing");
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

    /// A version seen twice is one row, which is what stops this table growing by a row per poll.
    #[test]
    fn a_tool_version_is_keyed_on_the_version_rather_than_the_sighting() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES ('m', 'm', 'os', '1', 'x86_64', 'cpu', 1, 0, 0, 0)",
            [],
        )
        .unwrap();
        let insert = |ts: i64| {
            conn.execute(
                "INSERT INTO tool_versions (machine_id, ts, tool, version)
                 VALUES ('m', ?1, 'claude-code', '2.1.187')",
                [ts],
            )
        };
        insert(5_000).unwrap();
        assert!(
            insert(6_000).is_err(),
            "the version, not the instant, has to be the key"
        );
    }

    /// Retention filters on `ts` alone, and the planner has to use the index for it.
    #[test]
    fn retention_does_not_full_scan_the_sample_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT min(ts) FROM samples WHERE ts < 1",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_samples_ts"),
            "retention must not full-scan: {plan}"
        );
    }

    #[test]
    fn an_unrecognised_schema_version_is_refused_rather_than_touched() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", target_version() + 5)
            .unwrap();
        let error = migrate(&mut conn).unwrap_err().to_string();
        assert!(error.contains("not one this build understands"), "{error}");
        // The remedy has to be in the message: the likeliest cause is a database from the development
        // line, whose counter had reached 5, and "upgrade AgentBench" would be advice that cannot work.
        assert!(error.contains("move the file aside"), "{error}");
    }
}
