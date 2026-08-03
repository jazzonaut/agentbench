//! The HTTP surface. The only module aware of `tiny_http`.
//!
//! Blocking and thread-per-request, matching the rest of the codebase: no async runtime is introduced
//! for what is a handful of local JSON endpoints and a few embedded files.

pub mod assets;
pub mod handlers;
pub mod response;
pub mod router;

use crate::watch::{
    config::ServerConfig,
    store::{Level, Sink, Store},
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
    pub fn serve(self, store: &Store, sink: &Sink, shutdown: Arc<AtomicBool>) {
        while !shutdown.load(Ordering::Relaxed) {
            let request = match self.inner.recv_timeout(SHUTDOWN_POLL) {
                Ok(Some(request)) => request,
                Ok(None) => continue,
                Err(error) => {
                    sink.log(
                        Level::Warn,
                        "serve",
                        format!("accept failed, stopping HTTP server: {error}"),
                    );
                    return;
                }
            };
            let response = match store.reader() {
                Ok(reader) => router::route(&Req::parse(request.url()), &reader),
                Err(error) => Resp::error(503, &format!("database unavailable: {error}")),
            };
            if let Err(error) = respond(request, &response) {
                sink.log(Level::Warn, "serve", format!("failed to respond: {error}"));
            }
        }
    }
}

/// Write a [`Resp`] to a `tiny_http` request.
fn respond(request: tiny_http::Request, response: &Resp) -> std::io::Result<()> {
    let mut headers = vec![
        header("Content-Type", response.content_type),
        // The dashboard is local-only and embeds no third-party origins.
        header("X-Content-Type-Options", "nosniff"),
    ];
    headers.push(header(
        "Cache-Control",
        if response.immutable {
            "public, max-age=604800, immutable"
        } else {
            "no-store"
        },
    ));
    request.respond(tiny_http::Response::new(
        tiny_http::StatusCode(response.status),
        headers,
        std::io::Cursor::new(response.body.clone()),
        Some(response.body.len()),
        None,
    ))
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    // Both sides are compile-time constants from this module, so parsing cannot fail in practice.
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| panic!("invalid static header {name}"))
}
