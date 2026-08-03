//! `GET /api/verdicts` — today against the days before it.
//!
//! The window is the configured one rather than a request parameter. A baseline is a claim about what
//! normal looks like on this machine, and letting a caller widen the window until the verdict changed
//! would turn that claim into whatever the reader wanted to see.

use crate::watch::{
    analysis,
    serve::response::{Req, Resp},
    store::Reader,
};

pub fn handle(_req: &Req, reader: &Reader, window_days: u32) -> Resp {
    match analysis::today_against_baseline(reader, window_days) {
        Ok(comparisons) => Resp::json(&comparisons),
        Err(error) => Resp::error(500, &format!("verdict query failed: {error}")),
    }
}
