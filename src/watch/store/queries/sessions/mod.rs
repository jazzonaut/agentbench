//! Reading the session stream: today's activity, and how far the importer has got.

pub mod series;

pub use series::{Bucket, SessionSeries, session_buckets, session_series};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

/// Today's session activity, for the live tiles.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Today {
    pub turns: i64,
    pub tool_calls: i64,
    pub sessions: i64,
    pub projects: i64,
    pub output_tokens: i64,
    /// Absent rather than zero when nothing has happened yet: no data is not a value.
    pub cache_hit_ratio: Option<f64>,
    pub tool_read_p50_ms: Option<f64>,
    pub last_activity_ts: Option<i64>,
}

/// One transcript's recorded import position.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatermarkRow {
    pub path: String,
    pub size: i64,
    pub mtime: i64,
}

/// Session activity since `since_ms`.
///
/// The caller decides when the day started, because only the caller knows the local time zone.
pub fn today(conn: &Connection, machine_id: &str, since_ms: i64, now_ms: i64) -> Result<Today> {
    let (turns, sessions, projects, output_tokens, last_turn): (i64, i64, i64, i64, Option<i64>) =
        conn.query_row(
            "SELECT count(*), count(DISTINCT session_id), count(DISTINCT project),
                    coalesce(sum(output_tokens), 0), max(ts)
               FROM session_turns WHERE machine_id = ?1 AND ts >= ?2",
            rusqlite::params![machine_id, since_ms],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .context("count today's turns")?;
    let (tool_calls, last_tool): (i64, Option<i64>) = conn
        .query_row(
            "SELECT count(*), max(ts) FROM session_tools WHERE machine_id = ?1 AND ts >= ?2",
            rusqlite::params![machine_id, since_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("count today's tool calls")?;

    // One bucket spanning the whole day, so the day's figure and the chart's points come from the
    // same code and cannot drift apart.
    let whole_day = (now_ms - since_ms).max(1) + 1;
    let single = |series: SessionSeries| -> Result<Option<f64>> {
        Ok(
            session_series(conn, machine_id, series, since_ms, now_ms, whole_day)?
                .first()
                .map(|point| point.value),
        )
    };

    Ok(Today {
        turns,
        tool_calls,
        sessions,
        projects,
        output_tokens,
        cache_hit_ratio: single(SessionSeries::CacheHitRatio)?,
        tool_read_p50_ms: single(SessionSeries::ToolReadMs)?,
        last_activity_ts: last_turn.max(last_tool),
    })
}

/// Every recorded import position.
pub fn watermarks(conn: &Connection) -> Result<Vec<WatermarkRow>> {
    let mut statement = conn.prepare("SELECT path, size, mtime FROM import_watermark")?;
    let mut rows = statement.query([])?;
    let mut marks = Vec::new();
    while let Some(row) = rows.next()? {
        marks.push(WatermarkRow {
            path: row.get(0)?,
            size: row.get(1)?,
            mtime: row.get(2)?,
        });
    }
    Ok(marks)
}

/// Number of transcripts with a recorded position.
pub fn imported_files(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT count(*) FROM import_watermark", [], |row| {
        row.get(0)
    })
    .context("count imported transcripts")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::watch::store::migrations;

    pub const MACHINE: &str = "machine-under-test";
    pub const MINUTE: i64 = 60_000;

    /// A migrated in-memory database holding one hour of plausible session activity.
    ///
    /// Rows are inserted directly rather than through the importer: these tests are about what the
    /// queries make of the data, and a fixture that had to be derived from JSON first would fail for
    /// two unrelated reasons.
    pub fn fixture() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES (?1, ?1, 'TestOS', '1', 'x86_64', 'Test CPU', 8, 0, 0, 0)",
            [MACHINE],
        )
        .unwrap();

        // Six read-only calls: five in the first minute, one slow one in the second.
        let reads = [
            (1_000, "Read", 8, 1),
            (2_000, "Read", 11, 1),
            (3_000, "Grep", 12, 1),
            (4_000, "Edit", 40, 1),
            (5_000, "Read", 1_200, 1),
            (MINUTE + 1_000, "Glob", 900, 1),
            // A Bash call that ran, and one that waited a minute for a permission that never came.
            (6_000, "Bash", 250, 1),
            (7_000, "Bash", 60_000, 0),
        ];
        for (index, (ts, tool, duration, ok)) in reads.iter().enumerate() {
            conn.execute(
                "INSERT INTO session_tools (uuid, machine_id, ts, project, tool, duration_ms, ok)
                 VALUES (?1, ?2, ?3, 'D:\\Work', ?4, ?5, ?6)",
                rusqlite::params![format!("tool-{index}"), MACHINE, ts, tool, duration, ok],
            )
            .unwrap();
        }

        // Two turns in one session: the first answers a prompt, the second continues from a tool
        // result and so has no response interval.
        let turns = [
            ("turn-1", "req-1", 500, Some(4_000), 100, 100, 900, 10),
            ("turn-2", "req-2", 30_000, None, 0, 200, 0, 0),
        ];
        for (uuid, request, ts, response, input, output, cache_read, cache_create) in turns {
            conn.execute(
                "INSERT INTO session_turns (uuid, machine_id, ts, project, branch, model, effort,
                     service_tier, first_response_ms, input_tokens, output_tokens, cache_read,
                     cache_create, session_id, request_id)
                 VALUES (?1, ?2, ?3, 'D:\\Work', 'main', 'claude-opus-5', 'high', 'standard',
                     ?4, ?5, ?6, ?7, ?8, 'session-1', ?9)",
                rusqlite::params![
                    uuid,
                    MACHINE,
                    ts,
                    response,
                    input,
                    output,
                    cache_read,
                    cache_create,
                    request
                ],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn today_summarises_the_activity_it_is_given() {
        let conn = fixture();
        let today = today(&conn, MACHINE, 0, 60 * MINUTE).unwrap();
        assert_eq!(today.turns, 2);
        assert_eq!(today.tool_calls, 8);
        assert_eq!(today.sessions, 1);
        assert_eq!(today.projects, 1);
        assert_eq!(today.output_tokens, 300);
        assert_eq!(today.cache_hit_ratio, Some(0.9));
        assert_eq!(today.tool_read_p50_ms, Some(40.0));
        assert_eq!(today.last_activity_ts, Some(MINUTE + 1_000));
    }

    #[test]
    fn a_day_that_started_after_the_activity_reports_nothing_rather_than_zeroes() {
        let conn = fixture();
        let today = today(&conn, MACHINE, 24 * 60 * MINUTE, 25 * 60 * MINUTE).unwrap();
        assert_eq!(today.turns, 0);
        assert_eq!(today.tool_calls, 0);
        assert_eq!(
            today.tool_read_p50_ms, None,
            "no calls is not a latency of zero"
        );
        assert_eq!(today.cache_hit_ratio, None);
        assert_eq!(today.last_activity_ts, None);
    }

    #[test]
    fn watermarks_round_trip_and_are_counted() {
        let conn = fixture();
        assert!(watermarks(&conn).unwrap().is_empty());
        assert_eq!(imported_files(&conn).unwrap(), 0);

        conn.execute(
            "INSERT INTO import_watermark (path, size, mtime, rows_ok, rows_error, updated)
             VALUES ('D:\\one.jsonl', 4096, 17, 40, 1, 99)",
            [],
        )
        .unwrap();
        let marks = watermarks(&conn).unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].path, "D:\\one.jsonl");
        assert_eq!(marks[0].size, 4096);
        assert_eq!(marks[0].mtime, 17);
        assert_eq!(imported_files(&conn).unwrap(), 1);
    }
}
