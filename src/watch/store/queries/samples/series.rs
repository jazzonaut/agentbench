//! The closed set of passive series, and the two tables each of them can be read from.
//!
//! A series names two columns rather than one: the raw column, and the column of the one-minute rollup
//! that outlives it. Keeping both in one place is what stops the pair drifting — a series that charted
//! `cpu_percent` raw and `cpu_max` rolled up would show a machine that got busier fourteen days ago and
//! nothing on the page would say why.

use crate::watch::store::queries::Point;
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// How a minute of samples became one rolled-up number.
///
/// Reported to the client, because it changes what a point means. A rolled-up mean smooths; a rolled-up
/// maximum does the opposite, and a swap chart that switches from instantaneous to per-minute peak
/// halfway along would otherwise look like the machine started swapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    /// Mean over the minute. Used where the quantity is smooth and its average is the honest summary.
    Mean,
    /// Largest value in the minute. Used where a spike is the thing worth keeping.
    Max,
}

impl Reducer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::Max => "max",
        }
    }
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
    /// Bytes per second the agent's process tree wrote.
    ///
    /// **Absent on a discovery tick, not zero.** `sysinfo`'s first I/O delta for a newly seen process is its
    /// whole lifetime's traffic, so the tick after every discovery pass has no rate to report — the same
    /// priming rule the CPU delta follows, and the reason a rediscovered agent does not plant a spike.
    AgentWriteBytesS,
    /// Bytes per second the matched security scanners wrote.
    ///
    /// **This reads zero on Windows for the scanner most people have, and that is a property of the daemon
    /// staying unelevated rather than a defect.** An ordinary process sees a SYSTEM-owned process's CPU and
    /// not its I/O: Defender, Windows Update, the search indexer, `System` and `Registry` all reported
    /// exactly zero bytes across 36 seconds while reporting their CPU. The series is kept because it is not
    /// structurally zero — a user-owned scanner registers here — but the page's note for it has to say which
    /// case a flat line is, because a chart that reads zero for ever otherwise says "nothing to see".
    ///
    /// Whole-machine throughput, which does count those writers, is a probe covariate instead:
    /// [`CondSeries::DiskWriteBytesS`]. The two answer different questions and neither replaces the other.
    ///
    /// [`CondSeries::DiskWriteBytesS`]: crate::watch::store::queries::CondSeries::DiskWriteBytesS
    ScannerWriteBytesS,
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
            "agent_write_bytes_s" => Self::AgentWriteBytesS,
            "scanner_write_bytes_s" => Self::ScannerWriteBytesS,
            _ => return None,
        })
    }

    /// Column this series reads from `samples`. Never built from caller input.
    fn column(self) -> &'static str {
        match self {
            Self::CpuPercent => "cpu_percent",
            Self::UsedMemory => "used_memory",
            Self::UsedSwap => "used_swap",
            Self::ProcessCount => "process_count",
            Self::ScannerCpu => "scanner_cpu",
            Self::AgentCpu => "agent_cpu",
            Self::AgentRss => "agent_rss",
            Self::AgentWriteBytesS => "agent_write_bytes_s",
            Self::ScannerWriteBytesS => "scanner_write_bytes_s",
        }
    }

    /// Column this series reads from `samples_1m`, once the raw rows have been pruned.
    fn rollup_column(self) -> &'static str {
        match self {
            Self::CpuPercent => "cpu_avg",
            Self::UsedMemory => "used_memory_avg",
            Self::UsedSwap => "used_swap_max",
            Self::ProcessCount => "process_count_avg",
            Self::ScannerCpu => "scanner_cpu_max",
            Self::AgentCpu => "agent_cpu_max",
            Self::AgentRss => "agent_rss_max",
            Self::AgentWriteBytesS => "agent_write_bytes_s_max",
            Self::ScannerWriteBytesS => "scanner_write_bytes_s_max",
        }
    }

    /// What a rolled-up point of this series is a summary of.
    ///
    /// Chosen per series rather than uniformly: memory in use wants its average, because the average is
    /// what the machine was living with, while swap, scanner CPU and the write rates want their peak,
    /// because a thirty-second burst of any of those is the event and its mean over a minute hides it.
    pub fn reducer(self) -> Reducer {
        match self {
            Self::CpuPercent | Self::UsedMemory | Self::ProcessCount => Reducer::Mean,
            Self::UsedSwap
            | Self::ScannerCpu
            | Self::AgentCpu
            | Self::AgentRss
            | Self::AgentWriteBytesS
            | Self::ScannerWriteBytesS => Reducer::Max,
        }
    }

    /// Unit reported to the client, which derives its axis and tooltip from it.
    ///
    /// A closed vocabulary shared with every other family — `%`, `B`, `B/s`, `ms`, `ratio`, `tokens`, and
    /// the empty string for a bare count — so one formatter on the page serves all four and a switchable
    /// frame cannot keep the previous selection's axis. **The per-core scale is not encoded here**: the unit
    /// of [`ScannerCpu`] really is a percentage, and "of one core rather than of the machine" is a caveat
    /// for the note beside it, not a different unit.
    ///
    /// [`ScannerCpu`]: SampleSeries::ScannerCpu
    pub fn unit(self) -> &'static str {
        match self {
            Self::CpuPercent | Self::ScannerCpu | Self::AgentCpu => "%",
            Self::UsedMemory | Self::UsedSwap | Self::AgentRss => "B",
            Self::AgentWriteBytesS | Self::ScannerWriteBytesS => "B/s",
            Self::ProcessCount => "",
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
        Self::AgentWriteBytesS,
        Self::ScannerWriteBytesS,
    ];

    pub fn wire_name(self) -> &'static str {
        self.column()
    }
}

/// Newest-first points from `samples`, capped at `limit`.
pub(super) fn raw_points(
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
    read(conn, &sql, machine_id, from_ms, to_ms, limit)
}

/// Newest-first points from `samples_1m`, capped at `limit`.
///
/// A bucket is stamped with its start, which is where it belongs on a time axis: the minute beginning at
/// 09:04:00 is plotted at 09:04:00, not at the moment the rollup happened to run.
pub(super) fn rollup_points(
    conn: &Connection,
    machine_id: &str,
    series: SampleSeries,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
) -> Result<Vec<Point>> {
    let sql = format!(
        "SELECT bucket, {column} FROM samples_1m
          WHERE machine_id = ?1 AND bucket >= ?2 AND bucket <= ?3 AND {column} IS NOT NULL
          ORDER BY bucket DESC LIMIT ?4",
        column = series.rollup_column()
    );
    read(conn, &sql, machine_id, from_ms, to_ms, limit)
}

/// Run a `(ts, value)` statement and collect its rows in the order the statement returned them.
fn read(
    conn: &Connection,
    sql: &str,
    machine_id: &str,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
) -> Result<Vec<Point>> {
    let mut statement = conn.prepare_cached(sql)?;
    let mut rows = statement.query(rusqlite::params![machine_id, from_ms, to_ms, limit as i64])?;
    let mut points = Vec::new();
    while let Some(row) = rows.next()? {
        points.push(Point {
            ts: row.get(0)?,
            value: row.get(1)?,
        });
    }
    Ok(points)
}
