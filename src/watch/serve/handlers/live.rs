//! `GET /api/live` — the most recent observation, probe and session activity, for the live tiles.

use crate::watch::{
    clock,
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
    day_start_ts: i64,
    sample: Option<queries::Latest>,
    /// The most recent controlled measurement, absent until the first probe interval has elapsed.
    probe: Option<queries::LatestProbe>,
    today: Option<queries::Today>,
}

pub fn handle(_req: &Req, reader: &Reader) -> Resp {
    let sample = match queries::latest(reader.conn(), reader.machine_id()) {
        Ok(sample) => sample,
        Err(error) => return Resp::error(500, &format!("live query failed: {error}")),
    };
    let now = crate::watch::store::now_ms();
    let day_start = clock::local_day_start_ms();
    // Session history is optional: a machine that has never run Claude Code still has live tiles, so
    // a failure here reports no activity rather than taking the whole payload down.
    let today = queries::sessions::today(reader.conn(), reader.machine_id(), day_start, now).ok();
    // Same reasoning as `today`: probing can be switched off entirely, so its absence is a state the
    // page renders rather than an error that takes the live tiles down with it.
    let probe = queries::latest_run(reader.conn(), reader.machine_id())
        .ok()
        .flatten();
    Resp::json(&Live {
        machine_id: reader.machine_id(),
        server_ts: now,
        day_start_ts: day_start,
        sample,
        probe,
        today,
    })
}
