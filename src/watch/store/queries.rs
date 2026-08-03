//! Read-side queries.
//!
//! All SQL lives here, so handlers stay free of it and can be tested against plain structs. Every
//! function takes a read-only connection and returns owned domain data.

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

/// The most recent observation, for the live tiles.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Latest {
    pub ts: i64,
    pub cpu_percent: f64,
    pub used_memory: i64,
    pub total_memory: i64,
    pub used_swap: i64,
    pub process_count: i64,
    pub scanner_cpu: Option<f64>,
    pub agent_cpu: Option<f64>,
    pub agent_rss: Option<i64>,
    pub agent_processes: Option<i64>,
}

/// Counts and freshness used by `--status` and the health tile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub samples: i64,
    pub probe_runs: i64,
    pub session_turns: i64,
    pub session_tools: i64,
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

/// Series a chart can request. A closed set, so a request cannot inject a column name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSeries {
    CpuPercent,
    UsedMemory,
    UsedSwap,
    ProcessCount,
    ScannerCpu,
    AgentCpu,
    AgentRss,
}

impl SampleSeries {
    /// Parse the wire name used by the dashboard.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "cpu_percent" => Self::CpuPercent,
            "used_memory" => Self::UsedMemory,
            "used_swap" => Self::UsedSwap,
            "process_count" => Self::ProcessCount,
            "scanner_cpu" => Self::ScannerCpu,
            "agent_cpu" => Self::AgentCpu,
            "agent_rss" => Self::AgentRss,
            _ => return None,
        })
    }

    /// Column this series reads. Never built from caller input.
    fn column(self) -> &'static str {
        match self {
            Self::CpuPercent => "cpu_percent",
            Self::UsedMemory => "used_memory",
            Self::UsedSwap => "used_swap",
            Self::ProcessCount => "process_count",
            Self::ScannerCpu => "scanner_cpu",
            Self::AgentCpu => "agent_cpu",
            Self::AgentRss => "agent_rss",
        }
    }

    /// Every series, for discovery by the dashboard.
    pub const ALL: &'static [Self] = &[
        Self::CpuPercent,
        Self::UsedMemory,
        Self::UsedSwap,
        Self::ProcessCount,
        Self::ScannerCpu,
        Self::AgentCpu,
        Self::AgentRss,
    ];

    pub fn wire_name(self) -> &'static str {
        self.column()
    }
}

/// Most recent sample for a machine.
pub fn latest(conn: &Connection, machine_id: &str) -> Result<Option<Latest>> {
    let mut statement = conn.prepare_cached(
        "SELECT ts, cpu_percent, used_memory, total_memory, used_swap, process_count,
                scanner_cpu, agent_cpu, agent_rss, agent_processes
           FROM samples WHERE machine_id = ?1 ORDER BY ts DESC LIMIT 1",
    )?;
    let mut rows = statement.query([machine_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(Latest {
        ts: row.get(0)?,
        cpu_percent: row.get(1)?,
        used_memory: row.get(2)?,
        total_memory: row.get(3)?,
        used_swap: row.get(4)?,
        process_count: row.get(5)?,
        scanner_cpu: row.get(6)?,
        agent_cpu: row.get(7)?,
        agent_rss: row.get(8)?,
        agent_processes: row.get(9)?,
    }))
}

/// Points for one series within a time range, newest last.
///
/// `limit` caps the row count so a month-wide request cannot return a million points to the browser.
pub fn series(
    conn: &Connection,
    machine_id: &str,
    series: SampleSeries,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
) -> Result<Vec<Point>> {
    // The column is chosen from a closed enum, never from caller input.
    let sql = format!(
        "SELECT ts, {column} FROM samples
          WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND {column} IS NOT NULL
          ORDER BY ts DESC LIMIT ?4",
        column = series.column()
    );
    let mut statement = conn.prepare_cached(&sql)?;
    let mut rows = statement.query(rusqlite::params![machine_id, from_ms, to_ms, limit as i64])?;
    let mut points = Vec::new();
    while let Some(row) = rows.next()? {
        points.push(Point {
            ts: row.get(0)?,
            value: row.get(1)?,
        });
    }
    points.reverse();
    Ok(points)
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
