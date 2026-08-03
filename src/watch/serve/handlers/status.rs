//! `GET /api/status` — daemon health, counts, and recent operational events.
//!
//! This is how a daemon hidden by a scheduler explains itself, so it is also the payload `--status`
//! prints on the command line.

use crate::watch::{
    serve::{
        assets,
        handlers::series,
        response::{Req, Resp},
    },
    store::{Reader, WriterHealth, queries},
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

/// Build the status payload from a reader.
///
/// Shared by the HTTP handler and the CLI so the two can never disagree. `writer` is present only when
/// the caller is the daemon itself; a second process reading the same file can say nothing about a
/// thread it does not own.
pub fn build(reader: &Reader, event_limit: usize, writer: Option<&WriterHealth>) -> Result<Status> {
    let health = queries::health(reader.conn(), reader.machine_id())?;
    let now = crate::watch::store::now_ms();
    let sample_age_ms = health.last_sample_ts.map(|ts| now - ts);
    let writer_running = writer.map(WriterHealth::is_running);
    Ok(Status {
        tool_version: env!("CARGO_PKG_VERSION"),
        uplot_version: assets::UPLOT_VERSION,
        server_ts: now,
        // A stopped writer is not collection that has gone quiet, so it is not allowed to read as
        // collection that is merely between samples.
        collecting: sample_age_ms.is_some_and(|age| age < STALE_AFTER_MS)
            && writer_running != Some(false),
        writer_running,
        sample_age_ms,
        health,
        series: series::known_series(),
        events: queries::recent_events(reader.conn(), event_limit)?,
    })
}

pub fn handle(req: &Req, reader: &Reader, writer: Option<&WriterHealth>) -> Resp {
    let limit = req.param_usize("events", 500).unwrap_or(EVENT_LIMIT);
    match build(reader, limit, writer) {
        Ok(status) => Resp::json(&status),
        Err(error) => Resp::error(500, &format!("status query failed: {error}")),
    }
}
