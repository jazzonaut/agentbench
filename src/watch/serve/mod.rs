//! The HTTP surface. The only module aware of `tiny_http`.
//!
//! Blocking and single-threaded: the accept loop handles each request inline, and no async runtime is
//! introduced for what is a handful of local JSON endpoints and a few embedded files served to one
//! viewer. The cost of that choice is that a slow query delays the next request, which is why the page
//! keeps the expensive endpoints off its fastest poll.

pub mod assets;
pub mod handlers;
pub mod origin;
pub mod response;
pub mod router;
pub mod runs;

use crate::watch::{
    config::{ServerConfig, WatchConfig},
    store::{Level, Sink, Store, WriterHealth},
    supervisor,
};
use anyhow::{Context, Result};
use response::{Method, Req, Resp};
use std::{
    net::SocketAddr,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// How often the accept loop checks for shutdown while idle.
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);

/// Largest request body accepted, in bytes.
///
/// Sized for the one thing that is uploaded: a pair of JSON reports, each tens of kilobytes with its samples
/// included. Eight mebibytes leaves room for a stress run's sample series several times over and still
/// bounds what one request can make this process allocate.
const MAX_BODY_BYTES: usize = 8 << 20;

/// Consecutive accept failures tolerated before the server stops trying.
///
/// An accept can fail transiently — a descriptor limit reached, a peer that aborted between the SYN and
/// the handshake — and one of those is no reason to spend the rest of the session without a dashboard.
/// Sustained failure is a different thing, and retrying it forever would log a line per attempt.
const ACCEPT_FAILURE_LIMIT: u32 = 5;

/// The little a handler needs to know that is not in the database.
///
/// Passed in rather than read from a global, and kept to what is genuinely needed: a handler that could
/// reach the whole configuration would eventually reach for the data directory or the bind address, and the
/// read-only guarantee on this layer is worth more than the convenience.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Trailing window the verdicts compare today against, in whole local days.
    pub baseline_window_days: u32,
    /// Slowest cadence at which samples are expected to arrive.
    ///
    /// Carried for one reason: a handler deciding whether collection has stalled has to know how long a
    /// healthy gap can be, and that is configuration. A fixed threshold made a claim about the
    /// configuration instead — at an idle cadence of six minutes, which `sample_interval_idle` permits, a
    /// perfectly healthy daemon between two idle samples was reported as stalled with a warning dot.
    pub idle_interval: Duration,
    /// Liveness of the writer, when the caller is the daemon that owns it.
    ///
    /// The one runtime fact a handler cannot read out of the database: a writer that has stopped leaves
    /// the file exactly as it was, so the page would otherwise draw a flat line and call it a quiet
    /// machine.
    pub writer: Option<WriterHealth>,
    /// The daemon's benchmark slot, when this daemon starts benchmarks.
    ///
    /// The second runtime handle to cross into this layer, and it crosses for the same reason as
    /// [`writer`]: it is a fact about the running process that no query can answer. Absent in three
    /// distinct situations, all of which the handlers treat alike — `watch.toml` turned runs off, the
    /// caller is `--status` reading somebody else's database, or the caller is a test.
    ///
    /// Note what this does *not* do to the read-only guarantee above. Handlers still cannot reach the
    /// configuration or the writer; what they gain is one object with four methods on it, and the only one
    /// that acts refuses unless the request cleared the write gate.
    ///
    /// [`writer`]: Self::writer
    pub runs: Option<Arc<runs::Registry>>,
}

impl From<&WatchConfig> for Settings {
    /// Extract the little a handler needs, rather than handing it the configuration.
    ///
    /// Takes the whole [`WatchConfig`] because the two fields come from two of its sections and a caller
    /// assembling them by hand is a caller that can forget one. What crosses into the read-only layer is
    /// still only these two values.
    fn from(config: &WatchConfig) -> Self {
        Self {
            baseline_window_days: config.analysis.baseline_window_days,
            idle_interval: config.collect.sample_interval_idle,
            writer: None,
            runs: None,
        }
    }
}

impl Default for Settings {
    /// The shipped window and cadence, for tests and for anything that has no configuration to hand.
    fn default() -> Self {
        Self {
            baseline_window_days: 7,
            idle_interval: Duration::from_secs(30),
            writer: None,
            runs: None,
        }
    }
}

impl Settings {
    /// Report this writer's liveness alongside the rest of the status payload.
    pub fn watching(mut self, writer: WriterHealth) -> Self {
        self.writer = Some(writer);
        self
    }

    /// Let the benchmark endpoints start runs through this registry.
    ///
    /// Not calling this is what disables them, and the endpoints then say so rather than disappearing.
    pub fn with_runs(mut self, runs: Arc<runs::Registry>) -> Self {
        self.runs = Some(runs);
        self
    }
}

/// Bound server, so the caller can report the real port before serving begins.
pub struct Server {
    inner: tiny_http::Server,
    address: SocketAddr,
}

impl Server {
    /// Bind the configured address.
    ///
    /// Binding before the collectors start means a port clash fails immediately and visibly rather
    /// than after the daemon appears to have started successfully.
    pub fn bind(config: &ServerConfig) -> Result<Self> {
        let address = SocketAddr::new(config.bind, config.port);
        let inner = tiny_http::Server::http(address)
            .map_err(|error| anyhow::anyhow!("could not bind {address}: {error}"))?;
        let address = inner
            .server_addr()
            .to_ip()
            .context("bound address is not an IP socket")?;
        Ok(Self { inner, address })
    }

    /// The address actually bound, which differs from the request when port 0 was configured.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// URL to open in a browser.
    pub fn url(&self) -> String {
        format!("http://{}/", self.address)
    }

    /// Serve until `shutdown` is set.
    ///
    /// Polls with a timeout rather than iterating `incoming_requests`, so Ctrl+C is honoured promptly
    /// instead of waiting for one more request to arrive.
    ///
    /// Each request gets a fresh read-only connection. At the volume a single local viewer generates,
    /// opening one per request is cheaper than the machinery of a pool.
    ///
    /// Returning means the dashboard is gone, not that the daemon is: the caller keeps collecting. Losing
    /// the page is an inconvenience; losing the history because the page's listener broke is the failure
    /// this daemon exists to avoid.
    pub fn serve(self, store: &Store, sink: &Sink, shutdown: Arc<AtomicBool>, settings: Settings) {
        let port = self.address.port();
        let mut accept_failures = 0_u32;
        while !shutdown.load(Ordering::Relaxed) {
            let request = match self.inner.recv_timeout(SHUTDOWN_POLL) {
                Ok(Some(request)) => request,
                // A timeout is proof the listener is healthy, so it clears the failure run too.
                Ok(None) => {
                    accept_failures = 0;
                    continue;
                }
                Err(error) => {
                    accept_failures += 1;
                    if accept_failures >= ACCEPT_FAILURE_LIMIT {
                        sink.log(
                            Level::Error,
                            "serve",
                            format!(
                                "accept failed {accept_failures} times in a row, so the dashboard is \
                                 no longer being served; collection continues: {error}"
                            ),
                        );
                        return;
                    }
                    sink.log(
                        Level::Warn,
                        "serve",
                        format!("accept failed, retrying: {error}"),
                    );
                    continue;
                }
            };
            accept_failures = 0;
            let mut request = request;
            // A panic in a handler must cost the request and not the daemon. Bound here rather than trusted
            // not to happen, for the same reason `Supervisor::spawn` bounds its worker body — and with more
            // at stake, because this runs on the main thread: an unwind out of `serve` leaves `run_with`
            // through the back door, and the collectors, the writer and the instance lock all go with it.
            //
            // `AssertUnwindSafe` needs stating. The reader is opened and dropped inside, the request is not
            // touched again on this path, and the one piece of shared state a handler can reach with an
            // invariant of its own is the run registry's mutex. A panic while that is held poisons it, and
            // every later benchmark request then answers 500 through this same boundary — a lost feature
            // rather than a lost daemon, which is the trade this is here to make.
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                dispatch(&mut request, port, store, &settings)
            }));
            let response = match outcome {
                Ok(response) => response,
                Err(payload) => {
                    sink.log(
                        Level::Error,
                        "serve",
                        format!(
                            "a request handler panicked and the dashboard answered 500; collection is \
                             unaffected: {} {}: {}",
                            request.method().as_str(),
                            request.url(),
                            supervisor::describe(&*payload)
                        ),
                    );
                    Resp::error(500, "the dashboard could not handle that request")
                }
            };
            if let Err(error) = respond(request, &response) {
                sink.log(Level::Warn, "serve", format!("failed to respond: {error}"));
            }
        }
    }
}

/// The gates, the body and the route for one request.
///
/// Three gates, in widening order of what they cost to check and narrowing order of what they let through.
/// All of them run before the database is opened.
///
/// Separate from the accept loop so the panic boundary has one call to wrap, rather than a closure holding
/// the whole loop body and therefore the failure counter it must not lose.
fn dispatch(
    request: &mut tiny_http::Request,
    port: u16,
    store: &Store,
    settings: &Settings,
) -> Resp {
    let method = Method::parse(request.method().as_str());
    if !origin::is_own_host(host_header(request), port) {
        // A rebound request is not a request for this server, and answering it at all is the whole of the
        // vulnerability.
        return Resp::error(
            421,
            "this dashboard answers only to its own loopback address",
        );
    }
    if method.is_write() && !write_is_permitted(request, port) {
        // Correctly addressed and still not ours: see `origin::is_same_origin_write`. A `403` rather than a
        // `421`, because the address was right and the provenance was not.
        return Resp::error(
            403,
            "a request that starts work must come from this dashboard's own page",
        );
    }
    let body = match read_body(request) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match store.reader() {
        Ok(reader) => {
            let req = Req::parse(request.url())
                .with_method(method)
                .with_body(body);
            router::route(&req, &reader, settings)
        }
        Err(error) => Resp::error(503, &format!("database unavailable: {error}")),
    }
}

/// The request's `Host`, if it sent one.
fn host_header(request: &tiny_http::Request) -> Option<&str> {
    header_value(request, "Host")
}

/// Whether a request that could change something proves it came from this dashboard.
///
/// The header reading is here and the rule is in [`origin::is_same_origin_write`], so the rule stays
/// testable without a socket — which is where the interesting cases are.
fn write_is_permitted(request: &tiny_http::Request, port: u16) -> bool {
    origin::is_same_origin_write(
        header_value(request, "Sec-Fetch-Site"),
        header_value(request, "Origin"),
        header_value(request, "Content-Type"),
        port,
    )
}

/// Read a request body, or the refusal to send instead of reading it.
///
/// Bounded, and bounded before anything is allocated: `Content-Length` is checked first so an enormous body
/// is refused without being read, and the read itself is capped as well because the header is the client's
/// claim rather than a fact. A report with its samples is tens of kilobytes, so the limit is generous by two
/// orders of magnitude and still finite — which is what stops a single request from exhausting the memory of
/// a daemon that is meant to be the least noticeable thing on the machine.
///
/// An absent length is a body of unknown size, not an empty one. `tiny_http` reports `None` for a chunked
/// request and decodes the chunks through the same reader, so the stream is read under the same cap. Treating
/// the absence as zero discarded the body and then blamed the document: a valid chunked `POST /api/compare`
/// was answered `unreadable report: EOF while parsing value`. Browsers' `fetch` sends a length for a string
/// body, so the dashboard's own pages never hit it — `fetch` with a *stream* body, and plenty of other
/// clients, do.
fn read_body(request: &mut tiny_http::Request) -> Result<Vec<u8>, Resp> {
    let declared = request.body_length();
    if declared.is_some_and(|length| length > MAX_BODY_BYTES) {
        return Err(Resp::error(
            413,
            &format!("a request body may not exceed {MAX_BODY_BYTES} bytes"),
        ));
    }
    if declared == Some(0) {
        return Ok(Vec::new());
    }
    // Only the declared length is trusted for the allocation, and only as a hint. An unknown length reserves
    // nothing and lets the read grow the buffer, which is what keeps `Transfer-Encoding: chunked` from being
    // an eight-mebibyte allocation for a request that turns out to carry twelve bytes.
    let mut body = Vec::with_capacity(declared.unwrap_or(0));
    // One byte past the limit, so a body whose declared length understated it is caught rather than silently
    // truncated into something that might still parse.
    let mut limited = std::io::Read::take(request.as_reader(), MAX_BODY_BYTES as u64 + 1);
    match std::io::Read::read_to_end(&mut limited, &mut body) {
        Ok(_) if body.len() > MAX_BODY_BYTES => Err(Resp::error(
            413,
            &format!("a request body may not exceed {MAX_BODY_BYTES} bytes"),
        )),
        Ok(_) => Ok(body),
        Err(error) => Err(Resp::error(
            400,
            &format!("could not read the request body: {error}"),
        )),
    }
}

/// Write a [`Resp`] to a `tiny_http` request.
fn respond(request: tiny_http::Request, response: &Resp) -> std::io::Result<()> {
    let mut headers = vec![
        header("Content-Type", response.content_type),
        // The dashboard is local-only and embeds no third-party origins.
        header("X-Content-Type-Options", "nosniff"),
        // Nothing here is meant to be loaded inside someone else's page, and nothing here loads from
        // anywhere else. Both framing headers, because the older one is what a browser without CSP
        // support honours. `'unsafe-inline'` for styles alone: the page carries one inline stylesheet
        // and no inline script, so scripts keep the strict rule.
        header("X-Frame-Options", "DENY"),
        header(
            "Content-Security-Policy",
            "default-src 'self'; style-src 'self' 'unsafe-inline'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    ];
    // `no-cache` permits storing and requires asking, which is what an unversioned asset URL needs: the
    // browser keeps its copy and finds out in one loopback round trip whether it is still the right one.
    // `immutable` here instead would let a week-old script run against today's markup — see [`Resp::etag`].
    let (cache_control, matched) = match &response.etag {
        Some(etag) => {
            headers.push(header("ETag", &format!("\"{etag}\"")));
            let matched = header_value(&request, "If-None-Match")
                .is_some_and(|value| response::matches_etag(value, etag));
            ("no-cache", matched)
        }
        None => ("no-store", false),
    };
    headers.push(header("Cache-Control", cache_control));
    // A refusal has to say what would have been accepted, and that is per-path now that some paths answer
    // `POST` and most do not. Carried on the response by [`Resp::method_not_allowed`] rather than guessed
    // from the status here, which is what used to advertise `GET, HEAD` on every path alike.
    if let Some(allow) = response.allow {
        headers.push(header("Allow", allow));
    }
    // A 304 carries no body, and its `Content-Length` must be absent rather than zero — a zero would tell
    // the browser the resource is now empty instead of unchanged.
    let (status, body) = if matched {
        (304, Vec::new())
    } else {
        (response.status, response.body.clone())
    };
    let length = (!matched).then_some(body.len());
    request.respond(tiny_http::Response::new(
        tiny_http::StatusCode(status),
        headers,
        std::io::Cursor::new(body),
        length,
        None,
    ))
}

/// One request header's value, if the request carries it.
///
/// `name` is `&'static str` because `tiny_http`'s case-insensitive comparison requires one; every caller
/// passes a literal anyway.
fn header_value<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    // Both sides are compile-time constants from this module, so parsing cannot fail in practice.
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| panic!("invalid static header {name}"))
}
