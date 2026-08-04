//! `GET /api/status` — daemon health, counts, and recent operational events.
//!
//! This is how a daemon hidden by a scheduler explains itself, so it is also the payload `--status`
//! prints on the command line.

use crate::watch::{
    serve::{
        Settings, assets,
        handlers::series,
        response::{Req, Resp},
    },
    store::{Reader, queries},
};
use anyhow::Result;
use serde::Serialize;
use std::time::Duration;

/// Events surfaced by default.
const EVENT_LIMIT: usize = 50;

/// Shortest gap since the last sample that can mean collection has stalled.
///
/// A floor rather than the threshold itself. At the shipped cadence a stall is obvious within two minutes,
/// and a faster cadence should not make the page cry stall over one skipped sample.
const STALE_FLOOR_MS: i64 = 120_000;

/// Samples this many idle intervals apart still count as collection running.
///
/// Two, so one missed sample is not a fault: a tick that lands late because the machine was busy or asleep
/// is the ordinary case on the machines this daemon runs on, not a stopped collector.
const STALE_AFTER_INTERVALS: u32 = 2;

/// Everything needed to answer "is it working?".
#[derive(Debug, Serialize)]
pub struct Status {
    pub tool_version: &'static str,
    pub uplot_version: &'static str,
    pub server_ts: i64,
    /// Milliseconds since the most recent sample, if any.
    pub sample_age_ms: Option<i64>,
    /// Whether collection appears to be running.
    pub collecting: bool,
    /// Whether the single writer is still draining records, where that can be known.
    ///
    /// `None` from the command line, which reads the file from a second process and has no writer of its
    /// own to ask about. `Some(false)` is the fault a flat line alone cannot distinguish from a quiet
    /// machine: collection has ended and the page is drawing history rather than the present.
    pub writer_running: Option<bool>,
    pub health: queries::Health,
    /// Every series name the dashboard may request, including the prefixed probe family.
    pub series: Vec<String>,
    pub events: Vec<queries::EventRow>,
}

/// Longest gap between samples that still counts as collection running.
///
/// Derived from the configured idle cadence rather than fixed, because that cadence is the user's to
/// choose: `sample_interval_idle` legitimately reaches minutes, and against a constant two minutes a
/// healthy daemon on a quiet machine reported `collecting: false` and the page drew
/// `stalled · last sample 4m ago` beside a warning dot. The constant was making a claim about the
/// configuration.
fn stale_after_ms(idle_interval: Duration) -> i64 {
    let window = idle_interval
        .saturating_mul(STALE_AFTER_INTERVALS)
        .as_millis();
    i64::try_from(window)
        .unwrap_or(i64::MAX)
        .max(STALE_FLOOR_MS)
}

/// Build the status payload from a reader.
///
/// Shared by the HTTP handler and the CLI so the two can never disagree. `settings.writer` is present only
/// when the caller is the daemon itself; a second process reading the same file can say nothing about a
/// thread it does not own.
pub fn build(reader: &Reader, event_limit: usize, settings: &Settings) -> Result<Status> {
    let health = queries::health(reader.conn(), reader.machine_id())?;
    let now = crate::watch::store::now_ms();
    let sample_age_ms = health.last_sample_ts.map(|ts| now - ts);
    let writer_running = settings.writer.as_ref().map(|writer| writer.is_running());
    let stale_after = stale_after_ms(settings.idle_interval);
    Ok(Status {
        tool_version: env!("CARGO_PKG_VERSION"),
        uplot_version: assets::UPLOT_VERSION,
        server_ts: now,
        // A stopped writer is not collection that has gone quiet, so it is not allowed to read as
        // collection that is merely between samples.
        collecting: sample_age_ms.is_some_and(|age| age < stale_after)
            && writer_running != Some(false),
        writer_running,
        sample_age_ms,
        health,
        series: series::known_series(),
        events: queries::recent_events(reader.conn(), event_limit)?,
    })
}

pub fn handle(req: &Req, reader: &Reader, settings: &Settings) -> Resp {
    let limit = req.param_usize("events", 500).unwrap_or(EVENT_LIMIT);
    match build(reader, limit, settings) {
        Ok(status) => Resp::json(&status),
        Err(error) => Resp::error(500, &format!("status query failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound follows a slow cadence up and never follows a fast one below the floor.
    #[test]
    fn the_staleness_bound_follows_the_idle_cadence_but_not_below_the_floor() {
        // The shipped cadence: two intervals is a minute, so the floor governs.
        assert_eq!(stale_after_ms(Duration::from_secs(30)), STALE_FLOOR_MS);
        assert_eq!(stale_after_ms(Duration::from_secs(1)), STALE_FLOOR_MS);
        // Six minutes idle — what a 60s active interval scales to — must not read as stalled at four.
        let six_minutes = stale_after_ms(Duration::from_secs(360));
        assert_eq!(six_minutes, 720_000);
        assert!(six_minutes > 4 * 60 * 1000);
    }

    /// A cadence long enough to overflow the arithmetic must still yield a usable bound.
    #[test]
    fn an_absurd_cadence_saturates_rather_than_wrapping() {
        assert!(stale_after_ms(Duration::MAX) > STALE_FLOOR_MS);
    }
}
