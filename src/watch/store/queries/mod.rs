//! Read-side queries.
//!
//! All SQL lives under this module, so handlers stay free of it and can be tested against plain
//! structs. Every function takes a read-only connection and returns owned domain data.
//!
//! Split by stream: [`samples`] answers the passive series, [`sessions`] the derived transcript
//! series. What stays here is shared by both — the point type, the health roll-up, and the
//! operational log.

pub mod samples;
pub mod sessions;

pub use samples::{Latest, SampleSeries, latest, series};
pub use sessions::{SessionSeries, Today, session_series};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

/// One point on a chart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Point {
    /// Milliseconds since the Unix epoch.
    pub ts: i64,
    pub value: f64,
}

/// Counts and freshness used by `--status` and the health tile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub samples: i64,
    pub probe_runs: i64,
    pub session_turns: i64,
    pub session_tools: i64,
    /// Transcripts with a recorded import position.
    pub imported_files: i64,
    pub import_errors: i64,
    pub last_sample_ts: Option<i64>,
    pub first_sample_ts: Option<i64>,
    pub schema_version: i64,
}

/// One operational log line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventRow {
    pub ts: i64,
    pub level: String,
    pub source: String,
    pub message: String,
}

/// Aggregate counts and freshness.
pub fn health(conn: &Connection, machine_id: &str) -> Result<Health> {
    let count = |sql: &str| -> Result<i64> {
        conn.query_row(sql, [machine_id], |row| row.get(0))
            .with_context(|| format!("count via {sql}"))
    };
    let (first, last): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT min(ts), max(ts) FROM samples WHERE machine_id = ?1",
        [machine_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Health {
        samples: count("SELECT count(*) FROM samples WHERE machine_id = ?1")?,
        probe_runs: count("SELECT count(*) FROM probe_runs WHERE machine_id = ?1")?,
        session_turns: count("SELECT count(*) FROM session_turns WHERE machine_id = ?1")?,
        session_tools: count("SELECT count(*) FROM session_tools WHERE machine_id = ?1")?,
        imported_files: sessions::imported_files(conn)?,
        import_errors: conn.query_row(
            "SELECT coalesce(sum(rows_error), 0) FROM import_watermark",
            [],
            |row| row.get(0),
        )?,
        first_sample_ts: first,
        last_sample_ts: last,
        schema_version: conn.query_row("PRAGMA user_version", [], |row| row.get(0))?,
    })
}

/// Most recent operational events, newest first.
pub fn recent_events(conn: &Connection, limit: usize) -> Result<Vec<EventRow>> {
    let mut statement = conn.prepare_cached(
        "SELECT ts, level, source, message FROM events ORDER BY id DESC LIMIT ?1",
    )?;
    let mut rows = statement.query([limit as i64])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(EventRow {
            ts: row.get(0)?,
            level: row.get(1)?,
            source: row.get(2)?,
            message: row.get(3)?,
        });
    }
    Ok(events)
}
