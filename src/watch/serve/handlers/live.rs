//! `GET /api/live` — the most recent observation and the most recent probe, for the live tiles.
//!
//! Everything here is one row per stream, which is what makes it affordable on the page's five-second poll.
//! The day's aggregates used to be part of this payload and are now [`today`], on the minute cadence, for
//! that reason.
//!
//! [`today`]: super::today

use crate::watch::{
    analysis::day,
    serve::response::{Req, Resp},
    store::{Reader, queries},
};
use serde::Serialize;

/// Live payload. `sample` is absent until the first observation lands.
#[derive(Debug, Serialize)]
struct Live<'a> {
    machine_id: &'a str,
    server_ts: i64,
    /// Local midnight, so the client labels the same day the server counted.
    ///
    /// Kept here as well as on `/api/today` because it costs no query: the page needs a day to name before
    /// the first aggregate arrives, and one derived from the browser's own clock could name a different one.
    day_start_ts: i64,
    sample: Option<queries::Latest>,
    /// The most recent controlled measurement, absent until the first probe interval has elapsed.
    ///
    /// Probing can be switched off entirely, so its absence is a state the page renders rather than an
    /// error that takes the live tiles down with it.
    probe: Option<queries::LatestProbe>,
}

pub fn handle(_req: &Req, reader: &Reader) -> Resp {
    let sample = match queries::latest(reader.conn(), reader.machine_id()) {
        Ok(sample) => sample,
        Err(error) => return Resp::error(500, &format!("live query failed: {error}")),
    };
    let probe = queries::latest_run(reader.conn(), reader.machine_id())
        .ok()
        .flatten();
    Resp::json(&Live {
        machine_id: reader.machine_id(),
        server_ts: crate::watch::store::now_ms(),
        // The same day boundary the baselines use, from the same place, so a tile and the verdict beside it
        // are never counting different days.
        day_start_ts: day::today().start_ms,
        sample,
        probe,
    })
}
