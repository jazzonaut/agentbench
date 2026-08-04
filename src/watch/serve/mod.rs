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

use crate::watch::{
    config::{ServerConfig, WatchConfig},
    store::{Level, Sink, Store, WriterHealth},
};
use anyhow::{Context, Result};
use response::{Req, Resp};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// How often the accept loop checks for shutdown while idle.
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);

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
        }
    }
}

impl Settings {
    /// Report this writer's liveness alongside the rest of the status payload.
    pub fn watching(mut self, writer: WriterHealth) -> Self {
        self.writer = Some(writer);
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
            // Checked before the database is even opened: a rebound request is not a request for this
            // server, and answering it at all is the whole of the vulnerability.
            let response = if origin::is_own_host(host_header(&request), port) {
                match store.reader() {
                    Ok(reader) => router::route(&Req::parse(request.url()), &reader, &settings),
                    Err(error) => Resp::error(503, &format!("database unavailable: {error}")),
                }
            } else {
                Resp::error(
                    421,
                    "this dashboard answers only to its own loopback address",
                )
            };
            if let Err(error) = respond(request, &response) {
                sink.log(Level::Warn, "serve", format!("failed to respond: {error}"));
            }
        }
    }
}

/// The request's `Host`, if it sent one.
fn host_header(request: &tiny_http::Request) -> Option<&str> {
    header_value(request, "Host")
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
