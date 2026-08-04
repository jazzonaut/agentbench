//! Read-side queries.
//!
//! All SQL lives under this module, so handlers stay free of it and can be tested against plain
//! structs. Every function takes a read-only connection and returns owned domain data.
//!
//! Split by stream: [`samples`] answers the passive series, [`sessions`] the derived transcript
//! series. What stays here is shared by both — the point type, the health roll-up, and the
//! operational log.

pub mod annotations;
pub mod probes;
pub mod samples;
pub mod sessions;

pub use annotations::{Annotation, AnnotationKind};
pub use probes::{
    CondSeries, LatestProbe, ProbeSeries, ProbeValue, cond_series, latest_run, probe_series,
};
pub use samples::{Latest, Reducer, Resolution, SampleSeries, SeriesRows, latest, series};
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

/// Points for one series, and whether the row budget ran out before the range did.
///
/// What [`SeriesRows`] is for the passive family, without the resolution and reducer that only a summarised
/// stream has. It exists because three families used to answer with a bare `Vec<Point>` and a `truncated` flag
/// hardcoded `false` at the handler, while their SQL capped the rows — so a range holding more points than the
/// cap was answered with a partial series that described itself as whole.
///
/// [`SeriesRows`]: samples::SeriesRows
#[derive(Debug, Clone, PartialEq)]
pub struct Points {
    /// Oldest first.
    pub points: Vec<Point>,
    /// True when the budget ran out before the range did.
    pub truncated: bool,
}

impl Points {
    /// Keep at most `limit` points, taken from the recent end, and say whether anything was dropped.
    ///
    /// One policy for every family, and it is the passive series' policy: asked for more than it can carry, a
    /// chart is better off showing the recent end of the range at full fidelity than a thinned version of the
    /// whole thing. The run-shaped families used to do the opposite by accident — `ORDER BY ts LIMIT n` keeps
    /// the *oldest* n — so the one thing a reader was certainly looking at was the first thing dropped.
    ///
    /// `points` arrives oldest first, which is the order every caller has in hand and the order the answer
    /// goes out in.
    pub(super) fn keep_recent(mut points: Vec<Point>, limit: usize) -> Self {
        let truncated = points.len() > limit;
        if truncated {
            points.drain(..points.len() - limit);
        }
        Self { points, truncated }
    }
}

/// Counts and freshness used by `--status` and the health tile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub samples: i64,
    pub probe_runs: i64,
    /// Probe runs that were not competing with anything, which is the subset a baseline can use.
    ///
    /// Reported beside the total because probing is ungated: on a busy week the comparable subset can be
    /// a small fraction of the runs collected, and a verdict computed from four points should say so.
    pub probe_runs_clean: i64,
    /// Foreground runs recorded as having loaded this machine.
    pub run_markers: i64,
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
///
/// Every count here is a scan of an index, so this is not a cheap query and is not meant to be asked
/// often — the dashboard polls it on the minute cadence, not the five-second one.
pub fn health(conn: &Connection, machine_id: &str) -> Result<Health> {
    let count = |sql: &str| -> Result<i64> {
        conn.query_row(sql, [machine_id], |row| row.get(0))
            .with_context(|| format!("count via {sql}"))
    };
    // Two statements rather than one. SQLite's min/max optimisation — seek one end of the index and
    // stop — applies only to a query with a single aggregate in it, so `SELECT min(ts), max(ts)` reads
    // every row for this machine while these two read one each.
    let extreme = |sql: &str| -> Result<Option<i64>> {
        conn.query_row(sql, [machine_id], |row| row.get(0))
            .with_context(|| format!("read via {sql}"))
    };
    let first = extreme("SELECT min(ts) FROM samples WHERE machine_id = ?1")?;
    let last = extreme("SELECT max(ts) FROM samples WHERE machine_id = ?1")?;
    Ok(Health {
        samples: count("SELECT count(*) FROM samples WHERE machine_id = ?1")?,
        probe_runs: count("SELECT count(*) FROM probe_runs WHERE machine_id = ?1")?,
        probe_runs_clean: count(
            "SELECT count(*) FROM probe_runs WHERE machine_id = ?1 AND contended = 0",
        )?,
        run_markers: count("SELECT count(*) FROM run_markers WHERE machine_id = ?1")?,
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
