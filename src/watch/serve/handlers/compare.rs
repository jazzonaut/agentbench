//! `POST /api/compare` — comparing two reports the page uploaded.
//!
//! The one write endpoint that changes nothing. It is a `POST` because two reports do not fit in a query
//! string, not because it has any effect: nothing here touches the database, the filesystem, or the
//! registry, and two identical requests produce two identical answers.
//!
//! No path is accepted, and that is the point of the design rather than a limitation. The browser reads the
//! two files the user picked and sends their contents; a server that took paths instead would be a loopback
//! endpoint that reads any file on the machine and returns whichever parts of it parse as a report.

use crate::{
    compare,
    model::Report,
    watch::serve::response::{Req, Resp},
};
use serde::Deserialize;

/// The two reports to compare.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    baseline: Report,
    candidate: Report,
}

pub fn handle(req: &Req) -> Resp {
    let request: Request = match req.json() {
        Ok(request) => request,
        // Names which document failed and why, because the page's next move is to tell the user which of the
        // two files they picked was not a report.
        Err(error) => return Resp::error(400, &format!("unreadable report: {error}")),
    };
    // The same version gate `report::read_report` applies to a report read from a path, reached through the
    // one function both call. Applied to each side separately so the message can say which file is the
    // problem rather than that one of them is.
    for (label, report) in [
        ("baseline", &request.baseline),
        ("candidate", &request.candidate),
    ] {
        if let Err(error) = compare::compatibility::ensure_supported_schema(report.schema_version) {
            return Resp::error(400, &format!("{label}: {error}"));
        }
    }
    match compare::compare_reports(&request.baseline, &request.candidate) {
        Ok(comparison) => Resp::json(&comparison),
        // An incomparable pair is the caller's mistake and the message is the whole answer: "presets differ:
        // quick vs standard" is what the page displays, unaltered.
        Err(error) => Resp::error(400, &format!("{error:#}")),
    }
}
