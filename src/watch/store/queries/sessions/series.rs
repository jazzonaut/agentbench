//! The derived session series.
//!
//! Every series here is an aggregate over a time bucket rather than a raw row, because one tool call
//! is not a measurement of the machine. A single `Read` of a cold 40 MB file says nothing; the middle
//! of an afternoon's reads says whether the filesystem got slower.
//!
//! Buckets are aligned to the epoch, not to a local day. Day boundaries are a reporting question and
//! a harder one — a local day is 23 or 25 hours twice a year — so they belong with the baselines that
//! need them rather than here.

use crate::watch::store::queries::{Point, Points};
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
    /// Latency of `Read`, and of nothing else: the clean filesystem signal.
    ///
    /// One tool per series because these four differ by more than an order of magnitude and the mix
    /// between them is decided by the model, not the machine. Measured over 15,035 real calls on one
    /// developer's machine: `Read` 11 ms, `Edit` 35 ms, `Grep` 72 ms, `Glob` 223 ms. Pooling them gave a
    /// daily median that correlated with the *share of calls that were reads* at r = −0.86 — three
    /// quarters of the movement in the one judged session series was composition — while the same days'
    /// `Read`-only medians correlated at −0.39. On 3 August the pooled figure sat near its worst for the
    /// month, 30 ms, on a day whose `Read` median was the best of it at 9.5 ms.
    ToolReadMs,
    /// Latency of `Edit` and `Write`. A filesystem cost too, but a write is what a scanner inspects and
    /// an `Edit` also carries the cost of matching what it is replacing.
    ToolEditMs,
    /// Latency of `Grep` and `Glob`: the closest thing here to a directory-walk measurement.
    ///
    /// Charted rather than judged because it scales with the size of the tree searched, so it moves when
    /// the agent changes project. That is also what makes it worth having: it is the series that would
    /// show a filter driver or a cloud-sync placeholder provider making enumeration expensive.
    ToolSearchMs,
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
    /// Output tokens per second of generation.
    ///
    /// **Only the requests that had a span to measure.** `generation_ms` is the interval from a request's
    /// first assistant row to its last, so a single-row request has none: 1,844 of 2,926 real requests
    /// emitted more than one row, leaving about 37% with nothing to measure. Those turns are excluded rather
    /// than divided by, which makes this a series about multi-row responses and not about all output. It is
    /// also end of *stream* rather than end of generation, and a response whose end nothing in the
    /// transcript proves has no span either — see `Deriver::finish`.
    ///
    /// A rate rather than a duration because a slow day and a verbose day are different things and the token
    /// count alone cannot tell them apart. Aggregated as a [`Aggregation::Ratio`] — tokens summed over
    /// seconds summed — so one two-token response cannot contribute the same weight as a thousand-token one.
    OutputTokensPerS,
    /// Share of prompt tokens served from the cache.
    CacheHitRatio,
}

impl SessionSeries {
    /// Parse the wire name used by the dashboard.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "tool_read_ms" => Self::ToolReadMs,
            "tool_edit_ms" => Self::ToolEditMs,
            "tool_search_ms" => Self::ToolSearchMs,
            "tool_bash_ms" => Self::ToolBashMs,
            "first_response_ms" => Self::FirstResponseMs,
            "output_tokens" => Self::OutputTokens,
            "output_tokens_per_s" => Self::OutputTokensPerS,
            "cache_hit_ratio" => Self::CacheHitRatio,
            _ => return None,
        })
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::ToolReadMs => "tool_read_ms",
            Self::ToolEditMs => "tool_edit_ms",
            Self::ToolSearchMs => "tool_search_ms",
            Self::ToolBashMs => "tool_bash_ms",
            Self::FirstResponseMs => "first_response_ms",
            Self::OutputTokens => "output_tokens",
            Self::OutputTokensPerS => "output_tokens_per_s",
            Self::CacheHitRatio => "cache_hit_ratio",
        }
    }

    /// Unit reported to the client, which derives its axis and tooltip from it.
    ///
    /// Drawn from the same closed vocabulary [`SampleSeries::unit`] documents. `ratio` is 0–1 rather than
    /// 0–100, which the page's formatter for that unit is what knows.
    ///
    /// [`SampleSeries::unit`]: crate::watch::store::queries::SampleSeries::unit
    pub fn unit(self) -> &'static str {
        match self {
            Self::ToolReadMs
            | Self::ToolEditMs
            | Self::ToolSearchMs
            | Self::ToolBashMs
            | Self::FirstResponseMs => "ms",
            Self::OutputTokens => "tokens",
            Self::OutputTokensPerS => "tokens/s",
            Self::CacheHitRatio => "ratio",
        }
    }

    /// Every series, for discovery by the dashboard.
    pub const ALL: &'static [Self] = &[
        Self::ToolReadMs,
        Self::ToolEditMs,
        Self::ToolSearchMs,
        Self::ToolBashMs,
        Self::FirstResponseMs,
        Self::OutputTokens,
        Self::OutputTokensPerS,
        Self::CacheHitRatio,
    ];

    fn aggregation(self) -> Aggregation {
        match self {
            Self::ToolReadMs
            | Self::ToolEditMs
            | Self::ToolSearchMs
            | Self::ToolBashMs
            | Self::FirstResponseMs => Aggregation::Median,
            Self::OutputTokens => Aggregation::Sum,
            Self::OutputTokensPerS | Self::CacheHitRatio => Aggregation::Ratio,
        }
    }

    /// Statement yielding `(ts, value, weight)` for this series, with no ordering or row cap of its own.
    ///
    /// Fixed text chosen by the enum, never assembled from a request. Failed, refused and interrupted
    /// tool calls are excluded from the latency series: each returned early or spent its time waiting
    /// for a person, so including them would make the machine look faster the more went wrong.
    ///
    /// The ordering and the cap are added by [`session_buckets`], because the two have to be applied in the
    /// opposite order from the one they read in: the cap keeps the *newest* rows and the bucketing needs them
    /// oldest first. Hence the aliases — the outer query selects these columns by name.
    fn sql(self) -> &'static str {
        match self {
            Self::ToolReadMs => {
                "SELECT ts, duration_ms AS value, 1 AS weight FROM session_tools
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND ok = 1 AND tool = 'Read'"
            }
            Self::ToolEditMs => {
                "SELECT ts, duration_ms AS value, 1 AS weight FROM session_tools
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND ok = 1
                    AND tool IN ('Edit', 'Write')"
            }
            Self::ToolSearchMs => {
                "SELECT ts, duration_ms AS value, 1 AS weight FROM session_tools
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND ok = 1
                    AND tool IN ('Grep', 'Glob')"
            }
            Self::ToolBashMs => {
                "SELECT ts, duration_ms AS value, 1 AS weight FROM session_tools
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND ok = 1 AND tool = 'Bash'"
            }
            Self::FirstResponseMs => {
                "SELECT ts, first_response_ms AS value, 1 AS weight FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3
                    AND first_response_ms IS NOT NULL"
            }
            Self::OutputTokens => {
                "SELECT ts, output_tokens AS value, 1 AS weight FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3"
            }
            // Seconds, not milliseconds, in the denominator: the ratio reducer divides the sums as they
            // come, so the unit of the result is decided here rather than by a later multiplication
            // nothing would connect to this statement.
            //
            // `generation_ms > 0` excludes more than a division by zero. A response whose first and last
            // rows share a millisecond has no measurable span, and admitting it would contribute an
            // enormous rate to the bucket from a request nothing was actually measured about.
            Self::OutputTokensPerS => {
                "SELECT ts, output_tokens AS value, generation_ms / 1000.0 AS weight
                   FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3
                    AND generation_ms IS NOT NULL AND generation_ms > 0"
            }
            Self::CacheHitRatio => {
                "SELECT ts, cache_read AS value, cache_read + input_tokens AS weight
                   FROM session_turns
                  WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3
                    AND cache_read + input_tokens > 0"
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

/// Bucketed points for one derived series, oldest first, capped at `limit` buckets.
///
/// The chart form: [`session_buckets`] without the weights, which no chart has a use for.
///
/// `limit` is honoured here and nowhere below, because a bucket count is a question about a chart rather than
/// about the data: a caller naming a one-minute bucket over a year is asking for half a million points, and the
/// endpoint's ceiling exists so that request cannot be answered literally. Left unapplied, as it was, the
/// endpoint read `limit` and then passed it only to the passive family.
pub fn session_series(
    conn: &Connection,
    machine_id: &str,
    series: SessionSeries,
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
    limit: usize,
) -> Result<Points> {
    let points = session_buckets(conn, machine_id, series, from_ms, to_ms, bucket_ms)?
        .into_iter()
        .map(|bucket| Point {
            ts: bucket.ts,
            value: bucket.value,
        })
        .collect();
    Ok(Points::keep_recent(points, limit))
}

/// Bucketed values for one derived series with their weights, oldest first.
///
/// [`MAX_ROWS`] bounds the rows read, and it is spent on the newest of them: the statement selects a
/// newest-first window and the wrapper sorts it back into time order, so a range holding more calls than the
/// budget summarises its recent end rather than its oldest. The plain `ORDER BY ts LIMIT` this replaced kept
/// the oldest instead, which for a chart is the opposite of what a reader is looking at.
pub fn session_buckets(
    conn: &Connection,
    machine_id: &str,
    series: SessionSeries,
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
) -> Result<Vec<Bucket>> {
    let bucket_ms = bucket_ms.max(1);
    // The inner text comes from the enum; only the ordering and the cap are added here.
    let sql = format!(
        "SELECT ts, value, weight FROM ({inner} ORDER BY ts DESC LIMIT ?4) ORDER BY ts",
        inner = series.sql()
    );
    let mut statement = conn.prepare_cached(&sql)?;
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

    /// A bucket budget wide enough not to be the thing under test.
    const UNBOUNDED: usize = 10_000;

    /// Every point of one series, with the bucket budget out of the way.
    fn points(conn: &Connection, series: SessionSeries, bucket_ms: i64) -> Vec<Point> {
        session_series(conn, MACHINE, series, 0, i64::MAX, bucket_ms, UNBOUNDED)
            .unwrap()
            .points
    }

    /// Values of one series, bucket by bucket.
    fn values(conn: &Connection, series: SessionSeries, bucket_ms: i64) -> Vec<f64> {
        points(conn, series, bucket_ms)
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
        // The first minute holds two quick reads and one that took over a second. Its mean would be
        // 406 ms and would report a filesystem problem that does not exist.
        assert_eq!(
            values(&conn, SessionSeries::ToolReadMs, MINUTE),
            vec![11.0],
            "the outlier must not move the median"
        );
    }

    /// One tool per series, because a `Glob` and a `Read` are not the same measurement.
    ///
    /// The fixture's numbers are the shape of the real ones: reads in milliseconds, an edit a few times
    /// slower, and a glob slower again by an order of magnitude. Pooling them, which is what this used
    /// to do, produced a median of 40 ms that described none of the three.
    #[test]
    fn each_tool_family_is_its_own_series() {
        let conn = fixture();
        let hour = 60 * MINUTE;
        assert_eq!(values(&conn, SessionSeries::ToolReadMs, hour), vec![11.0]);
        assert_eq!(values(&conn, SessionSeries::ToolEditMs, hour), vec![40.0]);
        // Grep at 12 ms and Glob at 900 ms; the p50 convention takes the upper middle of two.
        assert_eq!(
            values(&conn, SessionSeries::ToolSearchMs, hour),
            vec![900.0]
        );
    }

    #[test]
    fn buckets_split_by_time_and_are_ordered_oldest_first() {
        let conn = fixture();
        // The search series is the one spanning two minutes: a Grep in the first, a Glob in the second.
        let points = points(&conn, SessionSeries::ToolSearchMs, MINUTE);
        assert_eq!(points.len(), 2, "two minutes of calls, two buckets");
        assert!(points[0].ts < points[1].ts);
        assert_eq!(points[0].ts % MINUTE, 0, "buckets are aligned");
        // The second minute holds a single slow glob, which is its own median.
        assert_eq!(points[1].value, 900.0);
    }

    #[test]
    fn a_wider_bucket_merges_what_a_narrow_one_separates() {
        let conn = fixture();
        let narrow = values(&conn, SessionSeries::ToolSearchMs, MINUTE);
        assert_eq!(narrow.len(), 2, "two minutes, two buckets");
        let wide = values(&conn, SessionSeries::ToolSearchMs, 60 * MINUTE);
        assert_eq!(wide.len(), 1, "one hour, one bucket");
        assert_eq!(wide[0], 900.0, "the median of both searches");
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

    /// The rate covers only the requests that had a span, and is a ratio of the sums.
    ///
    /// The fixture's second turn has no `generation_ms`, which is the ordinary case for a single-row request
    /// and about 37% of real ones. Including it would mean dividing 200 tokens by nothing.
    #[test]
    fn tokens_per_second_uses_only_the_requests_with_a_measured_span() {
        let conn = fixture();
        // 100 tokens over 20 seconds. The 200-token turn has no span and contributes to neither side.
        assert_eq!(
            values(&conn, SessionSeries::OutputTokensPerS, 60 * MINUTE),
            vec![5.0]
        );

        // A second measured request, and the bucket becomes tokens summed over seconds summed rather than
        // the mean of two rates: 100/20 and 900/10 average to 47.5, while the honest figure is 33.3.
        conn.execute(
            "INSERT INTO session_turns (uuid, machine_id, ts, request_id, generation_ms, output_tokens)
             VALUES ('turn-3', ?1, 8000, 'req-3', 10000, 900)",
            [MACHINE],
        )
        .unwrap();
        let combined = values(&conn, SessionSeries::OutputTokensPerS, 60 * MINUTE);
        assert!(
            (combined[0] - 1000.0 / 30.0).abs() < 1e-9,
            "a ratio of sums, not a mean of ratios: {combined:?}"
        );
    }

    /// A request whose first and last row share a millisecond has no measurable rate.
    #[test]
    fn a_span_of_zero_is_excluded_rather_than_producing_an_enormous_rate() {
        let conn = fixture();
        conn.execute("UPDATE session_turns SET generation_ms = 0", [])
            .unwrap();
        assert!(
            values(&conn, SessionSeries::OutputTokensPerS, 60 * MINUTE).is_empty(),
            "nothing was measured, so there is no bucket"
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
        let points = session_series(
            &conn,
            MACHINE,
            SessionSeries::ToolReadMs,
            0,
            1,
            MINUTE,
            UNBOUNDED,
        )
        .unwrap();
        assert!(points.points.is_empty());
    }

    #[test]
    fn another_machines_rows_are_never_mixed_in() {
        let conn = fixture();
        let rows = session_series(
            &conn,
            "some-other-machine",
            SessionSeries::ToolReadMs,
            0,
            i64::MAX,
            MINUTE,
            UNBOUNDED,
        )
        .unwrap();
        assert!(rows.points.is_empty());
    }

    /// A bucket budget narrower than the range keeps the recent end and says so.
    ///
    /// Without this the endpoint's ceiling was decorative for three of its four families: a one-minute bucket
    /// over a year asks for half a million points, and the answer was as many as the row cap allowed with
    /// `truncated` hardcoded false.
    #[test]
    fn a_tight_bucket_budget_keeps_the_newest_buckets_and_reports_truncation() {
        let conn = fixture();
        let rows = session_series(
            &conn,
            MACHINE,
            SessionSeries::ToolSearchMs,
            0,
            i64::MAX,
            MINUTE,
            1,
        )
        .unwrap();
        assert!(rows.truncated);
        assert_eq!(rows.points.len(), 1);
        assert_eq!(
            rows.points[0].value, 900.0,
            "the later minute's glob, not the earlier minute's grep"
        );
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
        let reads = session_buckets(
            &conn,
            MACHINE,
            SessionSeries::ToolReadMs,
            0,
            i64::MAX,
            MINUTE,
        )
        .unwrap();
        assert_eq!(reads.len(), 1, "every read is in the first minute");
        assert_eq!(reads[0].observations, 3, "three reads behind that median");

        let buckets = session_buckets(
            &conn,
            MACHINE,
            SessionSeries::ToolSearchMs,
            0,
            i64::MAX,
            MINUTE,
        )
        .unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].observations, 1, "one grep in the first minute");
        assert_eq!(buckets[1].observations, 1);
        // The chart form must be the same numbers with the weights dropped, never a second computation.
        let points = points(&conn, SessionSeries::ToolSearchMs, MINUTE);
        assert_eq!(
            points.iter().map(|point| point.value).collect::<Vec<_>>(),
            buckets
                .iter()
                .map(|bucket| bucket.value)
                .collect::<Vec<_>>()
        );
    }
}
