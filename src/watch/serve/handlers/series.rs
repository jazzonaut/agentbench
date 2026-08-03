//! `GET /api/series` — points for one series over a time range.

use crate::watch::{
    serve::response::{Req, Resp},
    store::{Reader, queries},
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

#[derive(Debug, Serialize)]
struct Series<'a> {
    metric: &'a str,
    from: i64,
    to: i64,
    /// Gap threshold in milliseconds, for the client to break the line on.
    gap_ms: i64,
    truncated: bool,
    points: Vec<queries::Point>,
}

pub fn handle(req: &Req, reader: &Reader) -> Resp {
    let Some(name) = req.param("metric") else {
        return Resp::error(400, "metric is required");
    };
    let Some(series) = queries::SampleSeries::parse(name) else {
        let known: Vec<&str> = queries::SampleSeries::ALL
            .iter()
            .map(|s| s.wire_name())
            .collect();
        return Resp::error(
            400,
            &format!(
                "unknown metric {name:?}; known metrics: {}",
                known.join(", ")
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

    match queries::series(reader.conn(), reader.machine_id(), series, from, to, limit) {
        Ok(points) => Resp::json(&Series {
            metric: series.wire_name(),
            from,
            to,
            gap_ms: gap_threshold_ms(&points),
            truncated: points.len() >= limit,
            points,
        }),
        Err(error) => Resp::error(500, &format!("series query failed: {error}")),
    }
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
}
