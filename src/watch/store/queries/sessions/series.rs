//! The derived session series.
//!
//! Every series here is an aggregate over a time bucket rather than a raw row, because one tool call
//! is not a measurement of the machine. A single `Read` of a cold 40 MB file says nothing; the middle
//! of an afternoon's reads says whether the filesystem got slower.
//!
//! Buckets are aligned to the epoch, not to a local day. Day boundaries are a reporting question and
//! a harder one — a local day is 23 or 25 hours twice a year — so they belong with the baselines that
//! need them rather than here.

use crate::watch::store::queries::Point;
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Rows read into memory for one series request.
///
/// A month of heavy use is a few hundred thousand tool calls, which is bounded but not free, and no
/// chart is improved by more.
const MAX_ROWS: usize = 200_000;

/// How the values in a bucket become one number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aggregation {
    /// Middle value. Robust to the one enormous file read and the one command that hung.
    Median,
    /// Total. Says how much happened, not how fast it was.
    Sum,
    /// Sum of numerators over sum of denominators, never an average of ratios.
    Ratio,
}

/// A derived series a chart can request. A closed set, so no request builds SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSeries {
    /// Latency of the tools whose cost is the filesystem's: the clean signal.
    ToolReadMs,
    /// Latency of `Bash`. Available, but dominated by how long the command legitimately took and by
    /// waits for permission, so it is not a measure of the machine.
    ToolBashMs,
    /// Interval from a prompt to the first assistant message.
    ///
    /// Not a time to first token: it contains the whole thinking block, and a prompt typed while the
    /// agent was still working waits in a queue before the request is even sent.
    FirstResponseMs,
    /// Output tokens produced.
    OutputTokens,
    /// Share of prompt tokens served from the cache.
    CacheHitRatio,
}

impl SessionSeries {
    /// Parse the wire name used by the dashboard.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "tool_read_ms" => Self::ToolReadMs,
            "tool_bash_ms" => Self::ToolBashMs,
            "first_response_ms" => Self::FirstResponseMs,
            "output_tokens" => Self::OutputTokens,
            "cache_hit_ratio" => Self::CacheHitRatio,
            _ => return None,
        })
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::ToolReadMs => "tool_read_ms",
            Self::ToolBashMs => "tool_bash_ms",
            Self::FirstResponseMs => "first_response_ms",
            Self::OutputTokens => "output_tokens",
            Self::CacheHitRatio => "cache_hit_ratio",
        }
    }

    /// Every series, for discovery by the dashboard.
    pub const ALL: &'static [Self] = &[
        Self::ToolReadMs,
        Self::ToolBashMs,
        Self::FirstResponseMs,
        Self::OutputTokens,
        Self::CacheHitRatio,
    ];

    fn aggregation(self) -> Aggregation {
        match self {
            Self::ToolReadMs | Self::ToolBashMs | Self::FirstResponseMs => Aggregation::Median,
            Self::OutputTokens => Aggregation::Sum,
            Self::CacheHitRatio => Aggregation::Ratio,
        }
    }

    /// Statement yielding `(ts, numerator, denominator)` for this series.
    ///
    /// Fixed text chosen by the enum, never assembled from a request. Failed, refused and interrupted
    /// tool calls are excluded from the latency series: each returned early or spent its time waiting
    /// for a person, so including them would make the machine look faster the more went wrong.
    fn sql(self) -> &'static str {
        match self {
            Self::ToolReadMs => {
                "SELECT ts, duration_ms, 1 FROM session_tools
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND ok = 1
                    AND tool IN ('Read', 'Grep', 'Glob', 'Edit')
                  ORDER BY ts LIMIT ?4"
            }
            Self::ToolBashMs => {
                "SELECT ts, duration_ms, 1 FROM session_tools
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND ok = 1 AND tool = 'Bash'
                  ORDER BY ts LIMIT ?4"
            }
            Self::FirstResponseMs => {
                "SELECT ts, first_response_ms, 1 FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND first_response_ms IS NOT NULL
                  ORDER BY ts LIMIT ?4"
            }
            Self::OutputTokens => {
                "SELECT ts, output_tokens, 1 FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3
                  ORDER BY ts LIMIT ?4"
            }
            Self::CacheHitRatio => {
                "SELECT ts, cache_read, cache_read + input_tokens FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3
                    AND cache_read + input_tokens > 0
                  ORDER BY ts LIMIT ?4"
            }
        }
    }
}

/// One bucket of a derived series, and how many rows it was reduced from.
///
/// The weight is not decoration. A bucket's median is a fact about the machine only in proportion to what
/// it was computed from, and a baseline that treated a day of two tool calls as equal to a day of nine
/// hundred would be reporting the noise of the quiet day as a change in the machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub ts: i64,
    pub value: f64,
    pub observations: usize,
}

/// Bucketed points for one derived series, oldest first.
///
/// The chart form: [`session_buckets`] without the weights, which no chart has a use for.
pub fn session_series(
    conn: &Connection,
    machine_id: &str,
    series: SessionSeries,
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
) -> Result<Vec<Point>> {
    Ok(
        session_buckets(conn, machine_id, series, from_ms, to_ms, bucket_ms)?
            .into_iter()
            .map(|bucket| Point {
                ts: bucket.ts,
                value: bucket.value,
            })
            .collect(),
    )
}

/// Bucketed values for one derived series with their weights, oldest first.
pub fn session_buckets(
    conn: &Connection,
    machine_id: &str,
    series: SessionSeries,
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
) -> Result<Vec<Bucket>> {
    let bucket_ms = bucket_ms.max(1);
    let mut statement = conn.prepare_cached(series.sql())?;
    let mut rows = statement.query(rusqlite::params![
        machine_id,
        from_ms,
        to_ms,
        MAX_ROWS as i64
    ])?;

    // Rows arrive in time order, so a bucket is finished the moment a later one starts and nothing
    // has to be held but the bucket being filled.
    let mut points = Vec::new();
    let mut bucket: Option<i64> = None;
    let mut values: Vec<(f64, f64)> = Vec::new();
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let numerator: f64 = row.get(1)?;
        let denominator: f64 = row.get(2)?;
        let start = ts.div_euclid(bucket_ms) * bucket_ms;
        if bucket != Some(start) {
            if let Some(previous) = bucket.take() {
                points.push(reduce(previous, &mut values, series.aggregation()));
            }
            bucket = Some(start);
        }
        values.push((numerator, denominator));
    }
    if let Some(last) = bucket {
        points.push(reduce(last, &mut values, series.aggregation()));
    }
    Ok(points)
}

/// Collapse one bucket, clearing it for the next.
///
/// The median goes through [`crate::model::percentile`], which is where the tool's one percentile
/// convention lives: these values are read beside the p50s printed in benchmark reports, and a median
/// that meant something slightly different here would be a trap rather than a comparison.
fn reduce(bucket: i64, values: &mut Vec<(f64, f64)>, aggregation: Aggregation) -> Bucket {
    let value = match aggregation {
        Aggregation::Median => {
            let numerators: Vec<f64> = values.iter().map(|(numerator, _)| *numerator).collect();
            crate::model::percentile(&numerators, 0.5).unwrap_or(0.0)
        }
        Aggregation::Sum => values.iter().map(|(numerator, _)| numerator).sum(),
        Aggregation::Ratio => {
            let numerator: f64 = values.iter().map(|(num, _)| num).sum();
            let denominator: f64 = values.iter().map(|(_, den)| den).sum();
            if denominator > 0.0 {
                numerator / denominator
            } else {
                0.0
            }
        }
    };
    let observations = values.len();
    values.clear();
    Bucket {
        ts: bucket,
        value,
        observations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::queries::sessions::tests::{MACHINE, MINUTE, fixture};

    /// Values of one series, bucket by bucket.
    fn values(conn: &Connection, series: SessionSeries, bucket_ms: i64) -> Vec<f64> {
        session_series(conn, MACHINE, series, 0, i64::MAX, bucket_ms)
            .unwrap()
            .into_iter()
            .map(|point| point.value)
            .collect()
    }

    #[test]
    fn every_series_name_round_trips() {
        for series in SessionSeries::ALL {
            assert_eq!(
                SessionSeries::parse(series.wire_name()),
                Some(*series),
                "{}",
                series.wire_name()
            );
        }
        assert!(SessionSeries::parse("cpu_percent").is_none());
        assert!(SessionSeries::parse("").is_none());
    }

    /// The reason latency is a median: one enormous read must not become the minute's verdict.
    #[test]
    fn latency_is_the_middle_of_a_bucket_not_its_mean() {
        let conn = fixture();
        // The first minute holds four quick reads and one that took over a second. Its mean would be
        // 254 ms and would report a filesystem problem that does not exist.
        assert_eq!(
            values(&conn, SessionSeries::ToolReadMs, MINUTE),
            vec![12.0, 900.0],
            "the outlier must not move the median"
        );
    }

    #[test]
    fn buckets_split_by_time_and_are_ordered_oldest_first() {
        let conn = fixture();
        let points = session_series(
            &conn,
            MACHINE,
            SessionSeries::ToolReadMs,
            0,
            i64::MAX,
            MINUTE,
        )
        .unwrap();
        assert_eq!(points.len(), 2, "two minutes of calls, two buckets");
        assert!(points[0].ts < points[1].ts);
        assert_eq!(points[0].ts % MINUTE, 0, "buckets are aligned");
        // The second minute holds a single slow read, which is its own median.
        assert_eq!(points[1].value, 900.0);
    }

    #[test]
    fn a_wider_bucket_merges_what_a_narrow_one_separates() {
        let conn = fixture();
        let wide = values(&conn, SessionSeries::ToolReadMs, 60 * MINUTE);
        assert_eq!(wide.len(), 1, "one hour, one bucket");
        // Six reads of 8, 11, 12, 40, 900 and 1200 ms; the p50 convention takes the upper middle.
        assert_eq!(wide[0], 40.0, "the median of all six reads");
    }

    #[test]
    fn failed_and_refused_calls_are_excluded_from_latency() {
        let conn = fixture();
        // The fixture holds a refused Bash call that "took" a minute, and one that really ran.
        assert_eq!(
            values(&conn, SessionSeries::ToolBashMs, 60 * MINUTE),
            vec![250.0],
            "only the call that actually ran counts"
        );
    }

    #[test]
    fn tokens_are_summed_and_cache_hits_are_a_ratio_of_totals() {
        let conn = fixture();
        assert_eq!(
            values(&conn, SessionSeries::OutputTokens, 60 * MINUTE),
            vec![300.0]
        );
        // 900 cached of 1000 prompt tokens: the ratio of the totals, not the mean of two ratios.
        assert_eq!(
            values(&conn, SessionSeries::CacheHitRatio, 60 * MINUTE),
            vec![0.9]
        );
    }

    #[test]
    fn a_turn_without_a_measured_response_is_left_out_rather_than_counted_as_zero() {
        let conn = fixture();
        let responses = values(&conn, SessionSeries::FirstResponseMs, 60 * MINUTE);
        assert_eq!(responses, vec![4_000.0], "the continuation turn has none");
    }

    #[test]
    fn an_empty_range_yields_no_points_rather_than_an_error() {
        let conn = fixture();
        let points =
            session_series(&conn, MACHINE, SessionSeries::ToolReadMs, 0, 1, MINUTE).unwrap();
        assert!(points.is_empty());
    }

    #[test]
    fn another_machines_rows_are_never_mixed_in() {
        let conn = fixture();
        let points = session_series(
            &conn,
            "some-other-machine",
            SessionSeries::ToolReadMs,
            0,
            i64::MAX,
            MINUTE,
        )
        .unwrap();
        assert!(points.is_empty());
    }

    #[test]
    fn a_nonsensical_bucket_width_cannot_divide_by_zero() {
        let conn = fixture();
        assert!(!values(&conn, SessionSeries::ToolReadMs, 0).is_empty());
        assert!(!values(&conn, SessionSeries::ToolReadMs, -5).is_empty());
    }

    /// What a baseline needs and a chart does not: how much each bucket rests on.
    #[test]
    fn buckets_report_the_number_of_rows_behind_each_value() {
        let conn = fixture();
        let buckets = session_buckets(
            &conn,
            MACHINE,
            SessionSeries::ToolReadMs,
            0,
            i64::MAX,
            MINUTE,
        )
        .unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].observations, 5, "five reads in the first minute");
        assert_eq!(buckets[1].observations, 1);
        // The chart form must be the same numbers with the weights dropped, never a second computation.
        let points = session_series(
            &conn,
            MACHINE,
            SessionSeries::ToolReadMs,
            0,
            i64::MAX,
            MINUTE,
        )
        .unwrap();
        assert_eq!(
            points.iter().map(|point| point.value).collect::<Vec<_>>(),
            buckets
                .iter()
                .map(|bucket| bucket.value)
                .collect::<Vec<_>>()
        );
    }
}
