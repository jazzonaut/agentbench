//! `GET /api/status` — daemon health, counts, and recent operational events.
//!
//! This is how a daemon hidden by a scheduler explains itself, so it is also the payload `--status`
//! prints on the command line.

use crate::watch::{
    serve::{assets, response::Req, response::Resp},
    store::{Reader, queries},
};
use anyhow::Result;
use serde::Serialize;

/// Events surfaced by default.
const EVENT_LIMIT: usize = 50;

/// Longer than this since the last sample means collection has stalled.
const STALE_AFTER_MS: i64 = 120_000;

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
    pub health: queries::Health,
    pub series: Vec<&'static str>,
    pub events: Vec<queries::EventRow>,
}

/// Build the status payload from a reader.
///
/// Shared by the HTTP handler and the CLI so the two can never disagree.
pub fn build(reader: &Reader, event_limit: usize) -> Result<Status> {
    let health = queries::health(reader.conn(), reader.machine_id())?;
    let now = crate::watch::store::now_ms();
    let sample_age_ms = health.last_sample_ts.map(|ts| now - ts);
    Ok(Status {
        tool_version: env!("CARGO_PKG_VERSION"),
        uplot_version: assets::UPLOT_VERSION,
        server_ts: now,
        collecting: sample_age_ms.is_some_and(|age| age < STALE_AFTER_MS),
        sample_age_ms,
        health,
        series: queries::SampleSeries::ALL
            .iter()
            .map(|series| series.wire_name())
            .collect(),
        events: queries::recent_events(reader.conn(), event_limit)?,
    })
}

pub fn handle(req: &Req, reader: &Reader) -> Resp {
    let limit = req.param_usize("events", 500).unwrap_or(EVENT_LIMIT);
    match build(reader, limit) {
        Ok(status) => Resp::json(&status),
        Err(error) => Resp::error(500, &format!("status query failed: {error}")),
    }
}
