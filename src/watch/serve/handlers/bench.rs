//! `/api/bench` — describing, starting, watching and stopping a benchmark.
//!
//! The only handlers in this server that are not reads, and the only ones that need something the database
//! cannot supply: the registry on [`Settings::runs`]. When it is absent — because `watch.toml` disabled
//! runs, or because the caller is a test with no daemon behind it — every write here answers `503` and the
//! options endpoint answers with a form that explains why it is disabled. That is deliberately not a `404`:
//! the endpoint exists, and the page needs to be able to tell "this daemon will not do that" from "this
//! daemon is too old to know what you are asking".
//!
//! [`Settings::runs`]: crate::watch::serve::Settings::runs

use crate::watch::serve::{
    Settings,
    response::{Req, Resp},
    runs::{BenchRequest, Registry, StartRefusal},
};
use serde::Serialize;

/// Why a write was refused when no registry is present.
const DISABLED: &str = "this daemon does not start benchmarks; set server.allow_runs in watch.toml, or use \
     `agentbench bench` directly";

/// What `POST /api/bench` answers with.
#[derive(Debug, Serialize)]
struct Started {
    run_id: String,
}

/// `GET /api/bench/options` — the presets, their limits, and whether runs are allowed at all.
pub fn options(_req: &Req, settings: &Settings) -> Resp {
    match settings.runs.as_deref() {
        Some(registry) => Resp::json(&registry.options()),
        None => Resp::json(&Registry::refused_options(DISABLED)),
    }
}

/// `POST /api/bench` — validate a request and start it.
pub fn start(req: &Req, settings: &Settings) -> Resp {
    let Some(registry) = settings.runs.as_deref() else {
        return Resp::error(503, DISABLED);
    };
    let request: BenchRequest = match req.json() {
        Ok(request) => request,
        Err(error) => return Resp::error(400, &format!("unreadable request: {error}")),
    };
    // Validated against the daemon's own data directory, which is where a run that names no target measures.
    let valid = match request.validate(registry.data_dir()) {
        Ok(valid) => valid,
        Err(error) => return Resp::error(400, &format!("{error:#}")),
    };
    match registry.start(&valid) {
        Ok(run_id) => Resp::accepted(&Started { run_id }),
        // A run already in flight is the one refusal that is not the caller's mistake, and `409` is what
        // says so: the request was well formed and the machine is busy. The page shows the running run
        // rather than an error, so it needs to be able to tell this from a `400`.
        Err(StartRefusal::Busy(message)) => Resp::error(409, &message),
        // The daemon failing to start a run is not a conflict. This used to answer `409` for an unwritable
        // reports directory and for an executable that would not launch, which told the page to display "the
        // machine is busy" for a full disk.
        Err(StartRefusal::Failed(error)) => {
            Resp::error(500, &format!("could not start the benchmark: {error:#}"))
        }
    }
}

/// `GET /api/bench/run` — what the registry is doing.
pub fn run(_req: &Req, settings: &Settings) -> Resp {
    match settings.runs.as_deref() {
        Some(registry) => Resp::json(&registry.snapshot()),
        // Idle rather than an error: a page polling this endpoint on a daemon that cannot run benchmarks
        // should draw an idle form, and it has already been told why by the options endpoint.
        None => Resp::json(&crate::watch::serve::runs::RunState::Idle),
    }
}

/// `POST /api/bench/cancel` — stop the run in flight.
pub fn cancel(_req: &Req, settings: &Settings) -> Resp {
    let Some(registry) = settings.runs.as_deref() else {
        return Resp::error(503, DISABLED);
    };
    match registry.cancel() {
        Ok(()) => Resp::json(&serde_json::json!({ "cancelled": true })),
        Err(error) => Resp::error(409, &format!("{error:#}")),
    }
}
