//! `GET /api/series` — points for one series over a time range.
//!
//! Two kinds of series answer here. A passive one returns the samples themselves, because each is
//! already a measurement of the whole machine. A derived one returns an aggregate per time bucket,
//! because a single tool call is not a measurement of anything; the middle of an hour's calls is.

use crate::watch::{
    serve::response::{Req, Resp},
    store::{
        Reader,
        queries::{self, SampleSeries, SessionSeries},
    },
};
use serde::Serialize;

/// Largest number of points returned, so a month-wide request cannot flood the browser.
const MAX_POINTS: usize = 20_000;

/// Range used when the caller does not specify one.
const DEFAULT_WINDOW_MS: i64 = 48 * 60 * 60 * 1000;

/// Gaps wider than this many multiples of the sampling cadence are treated as missing data.
///
/// Charts must break the line across them: interpolating across a suspended laptop draws a confident
/// straight line through hours that were never observed.
const GAP_FACTOR: i64 = 3;

/// Buckets aimed at across the requested range. About one per two horizontal pixels.
const TARGET_BUCKETS: i64 = 120;

/// Shortest bucket a derived series will use.
///
/// Below a minute a bucket holds one or two tool calls, and the median of two numbers is not a
/// measurement — it is the noise the aggregation exists to remove.
const MIN_BUCKET_MS: i64 = 60_000;

/// Which series was asked for.
enum Requested {
    Sample(SampleSeries),
    Session(SessionSeries),
}

impl Requested {
    fn parse(name: &str) -> Option<Self> {
        SampleSeries::parse(name)
            .map(Self::Sample)
            .or_else(|| SessionSeries::parse(name).map(Self::Session))
    }

    fn wire_name(&self) -> &'static str {
        match self {
            Self::Sample(series) => series.wire_name(),
            Self::Session(series) => series.wire_name(),
        }
    }
}

/// Every series name the dashboard may ask for.
pub fn known_series() -> Vec<&'static str> {
    SampleSeries::ALL
        .iter()
        .map(|series| series.wire_name())
        .chain(SessionSeries::ALL.iter().map(|series| series.wire_name()))
        .collect()
}

#[derive(Debug, Serialize)]
struct Series<'a> {
    metric: &'a str,
    from: i64,
    to: i64,
    /// Gap threshold in milliseconds, for the client to break the line on.
    gap_ms: i64,
    /// Width of one aggregation bucket, or absent for a raw series.
    bucket_ms: Option<i64>,
    truncated: bool,
    points: Vec<queries::Point>,
}

pub fn handle(req: &Req, reader: &Reader) -> Resp {
    let Some(name) = req.param("metric") else {
        return Resp::error(400, "metric is required");
    };
    let Some(series) = Requested::parse(name) else {
        return Resp::error(
            400,
            &format!(
                "unknown metric {name:?}; known metrics: {}",
                known_series().join(", ")
            ),
        );
    };

    let now = crate::watch::store::now_ms();
    let to = req.param_i64("to").unwrap_or(now);
    let from = req.param_i64("from").unwrap_or(to - DEFAULT_WINDOW_MS);
    if from > to {
        return Resp::error(400, "from must not be after to");
    }
    let limit = req.param_usize("limit", MAX_POINTS).unwrap_or(MAX_POINTS);

    match series {
        Requested::Sample(sample) => {
            match queries::series(reader.conn(), reader.machine_id(), sample, from, to, limit) {
                Ok(points) => Resp::json(&Series {
                    metric: series.wire_name(),
                    from,
                    to,
                    gap_ms: gap_threshold_ms(&points),
                    bucket_ms: None,
                    truncated: points.len() >= limit,
                    points,
                }),
                Err(error) => Resp::error(500, &format!("series query failed: {error}")),
            }
        }
        Requested::Session(session) => {
            let bucket = req
                .param_i64("bucket")
                .filter(|bucket| *bucket > 0)
                .unwrap_or_else(|| bucket_ms(from, to));
            match queries::session_series(
                reader.conn(),
                reader.machine_id(),
                session,
                from,
                to,
                bucket,
            ) {
                Ok(points) => Resp::json(&Series {
                    metric: series.wire_name(),
                    from,
                    to,
                    // A bucket with no activity in it is a gap, not a zero: the agent was not working.
                    gap_ms: bucket.saturating_mul(GAP_FACTOR),
                    bucket_ms: Some(bucket),
                    truncated: false,
                    points,
                }),
                Err(error) => Resp::error(500, &format!("series query failed: {error}")),
            }
        }
    }
}

/// Bucket width for a range, wide enough that a bucket holds a usable number of calls.
fn bucket_ms(from: i64, to: i64) -> i64 {
    ((to - from) / TARGET_BUCKETS).max(MIN_BUCKET_MS)
}

/// Infer a gap threshold from the observed cadence.
///
/// Uses the median inter-point spacing rather than the configured interval, because the cadence
/// changes with machine idleness and history may span several configurations.
fn gap_threshold_ms(points: &[queries::Point]) -> i64 {
    if points.len() < 3 {
        return DEFAULT_WINDOW_MS;
    }
    let mut deltas: Vec<i64> = points.windows(2).map(|w| w[1].ts - w[0].ts).collect();
    deltas.sort_unstable();
    let median = deltas[deltas.len() / 2].max(1);
    median.saturating_mul(GAP_FACTOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::queries::Point;

    fn points(spacings: &[i64]) -> Vec<Point> {
        let mut ts = 0;
        let mut out = vec![Point { ts, value: 1.0 }];
        for step in spacings {
            ts += step;
            out.push(Point { ts, value: 1.0 });
        }
        out
    }

    #[test]
    fn too_few_points_fall_back_to_the_default_window() {
        assert_eq!(gap_threshold_ms(&[]), DEFAULT_WINDOW_MS);
        assert_eq!(gap_threshold_ms(&points(&[5_000])), DEFAULT_WINDOW_MS);
    }

    #[test]
    fn the_threshold_follows_the_median_cadence() {
        // Mostly 5s cadence with one long outage: the median must ignore the outage.
        let series = points(&[5_000, 5_000, 5_000, 8 * 60 * 60 * 1000, 5_000]);
        assert_eq!(gap_threshold_ms(&series), 15_000);
    }

    #[test]
    fn a_slower_cadence_widens_the_threshold() {
        let series = points(&[30_000, 30_000, 30_000, 30_000]);
        assert_eq!(gap_threshold_ms(&series), 90_000);
    }

    #[test]
    fn duplicate_timestamps_cannot_produce_a_zero_threshold() {
        let series = points(&[0, 0, 0, 0]);
        assert!(gap_threshold_ms(&series) > 0);
    }

    #[test]
    fn buckets_scale_with_the_range_but_never_get_too_thin() {
        let hour = 3_600_000;
        assert_eq!(
            bucket_ms(0, hour),
            MIN_BUCKET_MS,
            "an hour cannot be sliced finer"
        );
        assert_eq!(bucket_ms(0, 7 * 24 * hour), 7 * 24 * hour / TARGET_BUCKETS);
        assert!(
            bucket_ms(0, 0) >= MIN_BUCKET_MS,
            "an empty range is still valid"
        );
    }

    #[test]
    fn both_families_of_series_are_recognised_and_advertised() {
        assert!(matches!(
            Requested::parse("cpu_percent"),
            Some(Requested::Sample(_))
        ));
        assert!(matches!(
            Requested::parse("tool_read_ms"),
            Some(Requested::Session(_))
        ));
        assert!(Requested::parse("session_tools; DROP TABLE samples").is_none());

        let known = known_series();
        assert!(known.contains(&"cpu_percent"));
        assert!(known.contains(&"tool_read_ms"));
        assert!(known.contains(&"first_response_ms"));
    }
}
