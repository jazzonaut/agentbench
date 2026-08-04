//! The passive sample series: live tiles and the system charts.
//!
//! One series is read from two tables. Raw rows carry the recent past at the sampling cadence; once
//! retention has summarised and pruned them, the same series continues out of the one-minute rollup. A
//! request never says which it wants — it asks for a range, and the answer reports which tables the
//! points came from, because a rolled-up point is a summary of sixty seconds and not an observation.

pub mod series;

pub use series::{Reducer, SampleSeries};

use crate::watch::store::queries::Point;
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Width of one rolled-up bucket. Fixed by the schema, restated here for the client.
pub const ROLLUP_BUCKET_MS: i64 = 60_000;

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

/// Which tables a set of points was drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Every point is one observation.
    Raw,
    /// Every point summarises a minute.
    Rollup,
    /// The older part of the range is summarised and the newer part is not.
    ///
    /// Not an error and not worth avoiding — it is what a range spanning the retention boundary looks
    /// like — but the client is told, because the character of the line changes partway along it.
    Mixed,
}

/// Points for one series, and where they came from.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesRows {
    /// Oldest first.
    pub points: Vec<Point>,
    pub resolution: Resolution,
    /// How the rolled-up part of the range was summarised, when there is one.
    pub reducer: Option<Reducer>,
    /// True when the row budget ran out before the range did.
    pub truncated: bool,
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

/// Points for one series within a time range, oldest first, spanning the retention boundary.
///
/// `limit` caps the row count so a month-wide request cannot return a million points to the browser. The
/// budget is spent newest-first: asked for more than it can carry, a chart is better off showing the
/// recent end of the range at full fidelity than a thinned version of the whole thing.
pub fn series(
    conn: &Connection,
    machine_id: &str,
    kind: SampleSeries,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
) -> Result<SeriesRows> {
    // Where raw coverage begins. The pruner only deletes minutes it has already summarised, so the two
    // tables meet at this instant and never overlap — which is what lets the halves simply be
    // concatenated instead of deduplicated.
    let raw_from: Option<i64> = conn.query_row(
        "SELECT min(ts) FROM samples WHERE machine_id = ?1",
        [machine_id],
        |row| row.get(0),
    )?;
    let raw_start = raw_from.unwrap_or(i64::MAX);

    // One row past what the caller asked for. That extra row is the whole difference between "there is
    // more history than would fit" and "the range happened to hold exactly this many points": comparing
    // the returned count against the limit cannot tell those apart, and used to report a chart as missing
    // data it had all of.
    let capacity = limit.saturating_add(1);

    let mut points = if to_ms >= raw_start {
        series::raw_points(
            conn,
            machine_id,
            kind,
            from_ms.max(raw_start),
            to_ms,
            capacity,
        )?
    } else {
        Vec::new()
    };
    let raw_fetched = points.len();

    let budget = capacity.saturating_sub(raw_fetched);
    let rolled = if budget > 0 && from_ms < raw_start {
        series::rollup_points(
            conn,
            machine_id,
            kind,
            from_ms,
            to_ms.min(raw_start.saturating_sub(1)),
            budget,
        )?
    } else {
        Vec::new()
    };

    // Both halves arrive newest-first; appending the older half and reversing once yields oldest-first
    // across the join without sorting. Trimming before the reverse spends the budget newest-first, which
    // is the end a reader is looking at.
    points.extend(rolled);
    let truncated = points.len() > limit;
    points.truncate(limit);
    let raw_count = raw_fetched.min(points.len());
    let rolled_count = points.len() - raw_count;
    points.reverse();

    Ok(SeriesRows {
        resolution: match (raw_count, rolled_count) {
            (0, 0) => Resolution::Raw,
            (0, _) => Resolution::Rollup,
            (_, 0) => Resolution::Raw,
            _ => Resolution::Mixed,
        },
        reducer: (rolled_count > 0).then(|| kind.reducer()),
        truncated,
        points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::migrations;

    const MACHINE: &str = "machine-under-test";
    const MINUTE: i64 = 60_000;

    /// A database whose raw samples start at minute 10, with minutes 0–9 already summarised.
    ///
    /// This is the shape retention leaves behind, and the only shape in which the join between the two
    /// tables can be got wrong.
    fn fixture() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES (?1, ?1, 'TestOS', '1', 'x86_64', 'Test CPU', 8, 0, 0, 0)",
            [MACHINE],
        )
        .unwrap();

        for minute in 0..10 {
            conn.execute(
                "INSERT INTO samples_1m (machine_id, bucket, samples, cpu_avg, cpu_max,
                     used_memory_avg, used_swap_max, scanner_cpu_max, agent_cpu_max,
                     process_count_avg, agent_rss_max, agent_write_bytes_s_max,
                     scanner_write_bytes_s_max)
                 VALUES (?1, ?2, 12, ?3, 90.0, 1073741824, 4096, 0.5, 5.0, 400, 1048576,
                     2097152.0, 0.0)",
                rusqlite::params![MACHINE, minute * MINUTE, 10.0 + minute as f64],
            )
            .unwrap();
        }
        for step in 0..5 {
            conn.execute(
                "INSERT INTO samples (machine_id, ts, cpu_percent, used_memory, total_memory,
                     used_swap, process_count, scanner_cpu, agent_cpu, agent_rss, agent_processes,
                     agent_write_bytes_s, scanner_write_bytes_s)
                 VALUES (?1, ?2, ?3, 2147483648, 17179869184, 8192, 401, 0.7, 6.0, 2097152, 2,
                     4194304.0, 0.0)",
                rusqlite::params![MACHINE, 10 * MINUTE + step * 5_000, 50.0 + step as f64],
            )
            .unwrap();
        }
        conn
    }

    fn read(from: i64, to: i64, limit: usize) -> SeriesRows {
        series(
            &fixture(),
            MACHINE,
            SampleSeries::CpuPercent,
            from,
            to,
            limit,
        )
        .unwrap()
    }

    #[test]
    fn a_recent_range_is_raw_and_says_so() {
        let rows = read(10 * MINUTE, i64::MAX, 100);
        assert_eq!(rows.resolution, Resolution::Raw);
        assert_eq!(rows.reducer, None, "nothing was summarised");
        assert_eq!(
            rows.points.iter().map(|p| p.value).collect::<Vec<_>>(),
            vec![50.0, 51.0, 52.0, 53.0, 54.0]
        );
    }

    #[test]
    fn a_range_older_than_the_raw_rows_continues_out_of_the_rollup() {
        let rows = read(0, 5 * MINUTE, 100);
        assert_eq!(rows.resolution, Resolution::Rollup);
        assert_eq!(rows.reducer, Some(Reducer::Mean));
        assert_eq!(rows.points.len(), 6, "minutes 0 through 5");
        assert_eq!(rows.points[0].value, 10.0);
    }

    /// The join. One line, two tables, no duplicated or missing instant in the middle.
    #[test]
    fn a_range_spanning_the_retention_boundary_is_one_ordered_series() {
        let rows = read(0, i64::MAX, 100);
        assert_eq!(rows.resolution, Resolution::Mixed);
        assert_eq!(
            rows.points.len(),
            15,
            "ten summarised minutes and five samples"
        );
        assert!(
            rows.points.windows(2).all(|pair| pair[0].ts < pair[1].ts),
            "the join must not reorder or repeat an instant: {:?}",
            rows.points.iter().map(|p| p.ts).collect::<Vec<_>>()
        );
        assert_eq!(rows.points[0].ts, 0);
        assert_eq!(rows.points[9].ts, 9 * MINUTE, "the last summarised minute");
        assert_eq!(rows.points[10].ts, 10 * MINUTE, "the first raw sample");
    }

    /// The recent end is what a reader is looking at, so it is the end that survives the budget.
    #[test]
    fn a_tight_budget_keeps_the_newest_points_and_reports_truncation() {
        let rows = read(0, i64::MAX, 6);
        assert!(rows.truncated);
        assert_eq!(rows.points.len(), 6);
        assert_eq!(
            rows.points.last().map(|p| p.value),
            Some(54.0),
            "the newest sample is never the one dropped"
        );
        assert_eq!(
            rows.resolution,
            Resolution::Mixed,
            "five raw points leave one of the budget for the rollup"
        );
    }

    /// A range that exactly fills the budget is complete, not truncated.
    ///
    /// The old test — count against limit — cannot tell an exact fit from a cut, and reported the chart as
    /// missing history it had every point of.
    #[test]
    fn a_budget_that_exactly_fits_the_range_is_not_reported_as_truncated() {
        let rows = read(0, i64::MAX, 15);
        assert_eq!(rows.points.len(), 15, "the fixture holds fifteen points");
        assert!(!rows.truncated);

        // One point short of the range is the genuine case, and it still says so.
        let cut = read(0, i64::MAX, 14);
        assert_eq!(cut.points.len(), 14);
        assert!(cut.truncated);
    }

    /// Each series has to name a rollup column that exists, or its history ends at the boundary.
    #[test]
    fn every_advertised_series_can_be_read_from_both_tables() {
        let conn = fixture();
        for kind in SampleSeries::ALL {
            let rows = series(&conn, MACHINE, *kind, 0, i64::MAX, 100).unwrap();
            assert!(
                !rows.points.is_empty(),
                "{} returned nothing from either table",
                kind.wire_name()
            );
        }
    }

    /// A column that was NULL when observed stays absent rather than becoming a zero.
    #[test]
    fn an_unobserved_column_yields_no_point_for_that_instant() {
        let conn = fixture();
        // No scanner was ever found on this machine, in either table.
        conn.execute("UPDATE samples SET scanner_cpu = NULL", [])
            .unwrap();
        conn.execute("UPDATE samples_1m SET scanner_cpu_max = NULL", [])
            .unwrap();
        let rows = series(&conn, MACHINE, SampleSeries::ScannerCpu, 0, i64::MAX, 100).unwrap();
        assert!(rows.points.is_empty(), "{:?}", rows.points);
        assert_eq!(rows.resolution, Resolution::Raw, "an empty answer is raw");

        // The other series in the same rows are unaffected: absence is per column, not per sample.
        let cpu = series(&conn, MACHINE, SampleSeries::CpuPercent, 0, i64::MAX, 100).unwrap();
        assert_eq!(cpu.points.len(), 15);
    }

    #[test]
    fn a_database_with_no_raw_samples_left_still_charts() {
        let conn = fixture();
        conn.execute("DELETE FROM samples", []).unwrap();
        let rows = series(&conn, MACHINE, SampleSeries::CpuPercent, 0, i64::MAX, 100).unwrap();
        assert_eq!(rows.resolution, Resolution::Rollup);
        assert_eq!(rows.points.len(), 10);
    }

    #[test]
    fn another_machines_rows_are_never_mixed_in() {
        let conn = fixture();
        let rows = series(
            &conn,
            "someone-else",
            SampleSeries::CpuPercent,
            0,
            i64::MAX,
            100,
        )
        .unwrap();
        assert!(rows.points.is_empty());
    }

    #[test]
    fn unknown_series_names_are_rejected() {
        assert!(SampleSeries::parse("cpu_percent").is_some());
        assert!(SampleSeries::parse("used_memory; DROP TABLE samples").is_none());
        assert!(SampleSeries::parse("").is_none());
    }
}
