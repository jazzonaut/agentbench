//! `GET /api/series` — points for one series over a time range.
//!
//! Three kinds of series answer here, and the difference between them is what a single row means. A
//! passive sample is already a measurement of the whole machine, so it is returned as it was recorded. A
//! probe run is too — a controlled workload observed end to end — so it is also returned raw, optionally
//! restricted to the runs that were not competing with anything. A derived session series is neither: one
//! tool call is not a measurement of anything, so it is aggregated per time bucket.

use crate::watch::{
    serve::response::{Req, Resp},
    store::{
        Reader,
        queries::{self, ProbeSeries, Reducer, Resolution, SampleSeries, SessionSeries},
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
///
/// Probe names are tried last because they are the only prefixed family (`probe:` / `bench:`), so they
/// cannot collide with a passive or session name however the other two grow.
enum Requested {
    Sample(SampleSeries),
    Session(SessionSeries),
    Probe(ProbeSeries),
}

impl Requested {
    fn parse(name: &str) -> Option<Self> {
        SampleSeries::parse(name)
            .map(Self::Sample)
            .or_else(|| SessionSeries::parse(name).map(Self::Session))
            .or_else(|| ProbeSeries::parse(name).map(Self::Probe))
    }

    fn wire_name(&self) -> String {
        match self {
            Self::Sample(series) => series.wire_name().to_string(),
            Self::Session(series) => series.wire_name().to_string(),
            Self::Probe(series) => series.wire_name(),
        }
    }
}

/// Every series name the dashboard may ask for.
pub fn known_series() -> Vec<String> {
    SampleSeries::ALL
        .iter()
        .map(|series| series.wire_name().to_string())
        .chain(
            SessionSeries::ALL
                .iter()
                .map(|series| series.wire_name().to_string()),
        )
        .chain(queries::probes::known_series())
        .collect()
}

#[derive(Debug, Serialize)]
struct Series {
    metric: String,
    from: i64,
    to: i64,
    /// Gap threshold in milliseconds, for the client to break the line on.
    gap_ms: i64,
    /// Width of one aggregation bucket, or absent for a raw series.
    bucket_ms: Option<i64>,
    /// Unit and direction, for a probe series whose name is not one the page hardcodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lower_is_better: Option<bool>,
    /// Which tables a passive series was read from, absent where the question does not arise.
    ///
    /// Passive samples are the only stream retention summarises, so probe runs and derived session series
    /// have no resolution to report and say nothing rather than saying "raw" and implying a choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<Resolution>,
    /// How the rolled-up stretch of the range was summarised, when some of it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    rollup_reducer: Option<Reducer>,
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
                Ok(rows) => Resp::json(&Series {
                    metric: series.wire_name(),
                    from,
                    to,
                    // Derived from the points as they came back, so a range that crosses the retention
                    // boundary breaks its line on the change of cadence as well as on any real outage.
                    gap_ms: gap_threshold_ms(&rows.points),
                    bucket_ms: (rows.resolution != Resolution::Raw)
                        .then_some(queries::samples::ROLLUP_BUCKET_MS),
                    unit: None,
                    lower_is_better: None,
                    resolution: Some(rows.resolution),
                    rollup_reducer: rows.reducer,
                    truncated: rows.truncated,
                    points: rows.points,
                }),
                Err(error) => Resp::error(500, &format!("series query failed: {error}")),
            }
        }
        Requested::Probe(probe) => {
            // Defaults to every run. Restricting to the uncontended subset is what makes two days
            // comparable, but it is also what makes a busy week look like a week with no data, so the
            // choice belongs to whoever is reading rather than to this endpoint.
            let uncontended_only = req
                .param("contended")
                .is_some_and(|value| value == "exclude");
            match queries::probe_series(
                reader.conn(),
                reader.machine_id(),
                probe,
                from,
                to,
                uncontended_only,
            ) {
                Ok(points) => Resp::json(&Series {
                    metric: series.wire_name(),
                    from,
                    to,
                    // Probes are far enough apart that a missed one is a real gap worth breaking the
                    // line on, and the observed cadence is the only thing that knows the interval —
                    // configuration can have changed several times across the range.
                    gap_ms: gap_threshold_ms(&points),
                    bucket_ms: None,
                    unit: Some(probe.spec.unit),
                    lower_is_better: Some(probe.spec.lower_is_better),
                    resolution: None,
                    rollup_reducer: None,
                    truncated: false,
                    points,
                }),
                Err(error) => Resp::error(500, &format!("probe series query failed: {error}")),
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
                    unit: None,
                    lower_is_better: None,
                    resolution: None,
                    rollup_reducer: None,
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
/// Uses the median inter-point spacing rather than the configured interval, because the cadence changes with
/// machine idleness and a range may span several configurations.
///
/// A range whose cadence changed *within* it — a fortnight of quarter-hourly probes beside an afternoon at
/// `--probe-interval 1s` — has a median of a second or two, against which every one of the older points
/// looks like an outage and becomes an island between two breaks. That is a real effect and it is left
/// alone here, because the alternative was worse: a floor tied to the requested range drew a confident
/// straight line across a ninety-second daemon restart, since the request was for forty-eight hours while
/// the plot had auto-ranged to nine minutes. Breaking the line is the honest answer in both cases, and an
/// island is made visible by the chart's point markers rather than by loosening the threshold that decides
/// what counts as unobserved time.
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

    /// A restart in the middle of a range still breaks the line, whatever range was requested.
    ///
    /// The regression this guards against was a threshold floored at a fraction of the *requested* range: a
    /// forty-eight-hour request whose data spanned nine minutes got an hour-wide threshold, and drew a
    /// straight confident line across the minute and a half the daemon was not running.
    #[test]
    fn a_short_outage_in_a_dense_series_is_still_a_gap() {
        let series = points(&[1_000, 1_000, 1_000, 90_000, 1_000, 1_000]);
        let threshold = gap_threshold_ms(&series);
        assert!(
            threshold < 90_000,
            "a ninety-second outage must not be interpolated across: {threshold}"
        );
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
    fn every_family_of_series_is_recognised_and_advertised() {
        assert!(matches!(
            Requested::parse("cpu_percent"),
            Some(Requested::Sample(_))
        ));
        assert!(matches!(
            Requested::parse("tool_read_ms"),
            Some(Requested::Session(_))
        ));
        assert!(matches!(
            Requested::parse("probe:filesystem.small_file_ops_s"),
            Some(Requested::Probe(_))
        ));
        assert!(matches!(
            Requested::parse("bench:cpu.single_mops_s"),
            Some(Requested::Probe(_))
        ));
        assert!(Requested::parse("session_tools; DROP TABLE samples").is_none());
        // A probe metric with no source prefix is not a series: it would have to pick one silently.
        assert!(Requested::parse("filesystem.small_file_ops_s").is_none());

        let known = known_series();
        for expected in [
            "cpu_percent",
            "tool_read_ms",
            "first_response_ms",
            "probe:filesystem.small_file_ops_s",
            "bench:filesystem.small_file_ops_s",
        ] {
            assert!(
                known.contains(&expected.to_string()),
                "{expected} should be advertised"
            );
        }
    }

    /// The wire name a request used is echoed back, so a client can tell which series it received.
    #[test]
    fn a_series_reports_the_name_it_was_asked_for() {
        for name in [
            "cpu_percent",
            "tool_read_ms",
            "probe:cpu.single_mops_s",
            "bench:cpu.single_mops_s",
        ] {
            let requested = Requested::parse(name).expect(name);
            assert_eq!(requested.wire_name(), name);
        }
    }
}
