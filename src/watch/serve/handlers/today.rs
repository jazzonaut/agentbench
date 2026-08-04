//! `GET /api/today` — the day's agent activity, aggregated since local midnight.
//!
//! Split out of `/api/live` rather than left riding its five-second poll. Everything here is a scan of the
//! day: two aggregates over `session_turns` and `session_tools`, plus two `session_series` passes for the
//! median and the cache ratio. None of it can move faster than the importer that feeds it, which polls
//! every thirty seconds at best, and the server answers one request at a time on its own thread — so
//! recomputing the day twelve times a minute was the dashboard adding load to the machine it is measuring.
//! The same argument that moved `/api/status` and `/api/verdicts` to the minute cadence.
//!
//! The one number here that does change continuously — how long ago the last activity was — is a timestamp,
//! so the page ticks it against its own clock between refreshes rather than asking again.

use crate::watch::{
    analysis::day,
    serve::response::{Req, Resp},
    store::{Reader, queries},
};
use serde::Serialize;

/// Today's activity payload.
#[derive(Debug, Serialize)]
struct Today {
    /// Local midnight, so the client labels the same day the server counted.
    day_start_ts: i64,
    /// Absent when the session history could not be read, which is a page with no agent tiles rather
    /// than an error: a machine may simply never have run Claude Code.
    today: Option<queries::Today>,
}

pub fn handle(_req: &Req, reader: &Reader) -> Resp {
    // The same day boundary the baselines use, from the same place, so a tile and the verdict beside it
    // are never counting different days.
    let day_start = day::today().start_ms;
    let now = crate::watch::store::now_ms();
    Resp::json(&Today {
        day_start_ts: day_start,
        today: queries::sessions::today(reader.conn(), reader.machine_id(), day_start, now).ok(),
    })
}
