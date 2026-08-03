//! `GET /api/live` — the most recent observation, for the live tiles.

use crate::watch::{
    serve::response::{Req, Resp},
    store::{Reader, queries},
};
use serde::Serialize;

/// Live payload. `sample` is absent until the first observation lands.
#[derive(Debug, Serialize)]
struct Live<'a> {
    machine_id: &'a str,
    server_ts: i64,
    sample: Option<queries::Latest>,
}

pub fn handle(_req: &Req, reader: &Reader) -> Resp {
    match queries::latest(reader.conn(), reader.machine_id()) {
        Ok(sample) => Resp::json(&Live {
            machine_id: reader.machine_id(),
            server_ts: crate::watch::store::now_ms(),
            sample,
        }),
        Err(error) => Resp::error(500, &format!("live query failed: {error}")),
    }
}
