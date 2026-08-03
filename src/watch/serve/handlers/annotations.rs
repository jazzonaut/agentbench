//! `GET /api/annotations` — what changed, for drawing on the charts.
//!
//! Takes the same `from`/`to` a series request takes, so a page can ask for marks over exactly the range it
//! just plotted rather than trying to line up two different windows.

use crate::watch::{
    serve::response::{Req, Resp},
    store::{Reader, queries},
};
use serde::Serialize;

/// Range used when the caller does not specify one. Matches `/api/series`.
const DEFAULT_WINDOW_MS: i64 = 48 * 60 * 60 * 1000;

#[derive(Debug, Serialize)]
struct Annotations {
    from: i64,
    to: i64,
    annotations: Vec<queries::Annotation>,
}

pub fn handle(req: &Req, reader: &Reader) -> Resp {
    let now = crate::watch::store::now_ms();
    let to = req.param_i64("to").unwrap_or(now);
    let from = req.param_i64("from").unwrap_or(to - DEFAULT_WINDOW_MS);
    if from > to {
        return Resp::error(400, "from must not be after to");
    }
    match queries::annotations::in_range(reader.conn(), reader.machine_id(), from, to) {
        Ok(annotations) => Resp::json(&Annotations {
            from,
            to,
            annotations,
        }),
        Err(error) => Resp::error(500, &format!("annotation query failed: {error}")),
    }
}
