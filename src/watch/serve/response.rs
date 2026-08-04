//! Transport-independent request and response types.
//!
//! Handlers take a [`Req`] and return a [`Resp`], so they are ordinary functions testable without
//! binding a socket, and `tiny_http` appears in exactly one file.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The request methods this server recognises.
///
/// Not a general HTTP method type: what is not listed here collapses into one arm, because a `PUT` and a
/// `TRACE` are the same thing to a router that has no handler for either. `Head` is separate from `Get`
/// only because the transport has to know not to write a body; the router treats the two identically.
///
/// `Post` arrived with the benchmark and comparison pages, which are the first endpoints in this design
/// that are not reads. Everything reachable by it is gated twice — once here, by the router refusing a
/// method a path does not answer, and once in [`super::origin::is_same_origin_write`], which is what
/// stops a page on the internet from starting a benchmark on this machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Method {
    #[default]
    Get,
    Head,
    Post,
    Other,
}

impl Method {
    /// Recognise a method name. Method names are case-sensitive, unlike header field names.
    pub fn parse(name: &str) -> Self {
        match name {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            _ => Self::Other,
        }
    }

    /// Whether a request arriving by this method is asking only to be shown something.
    pub fn is_read(self) -> bool {
        matches!(self, Self::Get | Self::Head)
    }

    /// Whether a request arriving by this method could change something.
    ///
    /// The distinction the write gate turns on, and deliberately not the negation of [`is_read`]: an
    /// unrecognised method is neither a read nor a write, and it is refused for not being a method this
    /// server answers rather than smuggled through the gate as one.
    ///
    /// [`is_read`]: Self::is_read
    pub fn is_write(self) -> bool {
        matches!(self, Self::Post)
    }
}

/// A parsed request, reduced to what handlers actually need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Req {
    /// How the request arrived.
    pub method: Method,
    /// Path with the query string removed, e.g. `/api/series`.
    pub path: String,
    /// Decoded query parameters.
    pub query: BTreeMap<String, String>,
    /// Request body, empty for every read.
    ///
    /// Bytes rather than a `String`. A handler that wants JSON deserialises them; one handed a body that
    /// claimed to be UTF-8 and was not would have had the invalid sequences replaced with substitution
    /// characters before it could report the problem.
    pub body: Vec<u8>,
}

impl Req {
    /// Split a raw request target into path and decoded query parameters.
    ///
    /// The method defaults to `GET` and the body to empty, which is what a bare target means: every caller
    /// with a real method to report adds it with [`Req::with_method`], and a test naming only a path is
    /// asking about a read.
    pub fn parse(target: &str) -> Self {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Self {
            method: Method::Get,
            path: path.to_string(),
            query: parse_query(query),
            body: Vec::new(),
        }
    }

    /// A `POST` to `target` carrying `body`, which is what a test writes.
    pub fn post(target: &str, body: impl Into<Vec<u8>>) -> Self {
        Self::parse(target)
            .with_method(Method::Post)
            .with_body(body.into())
    }

    /// The same request, arriving by `method`.
    pub fn with_method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// The same request, carrying `body`.
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Deserialise the body as JSON, or say why it could not be.
    ///
    /// The message is handed back rather than logged, because a malformed body is the client's mistake and
    /// the page that made it is the only thing that can act on it. Serde's own wording names the field and
    /// the position, which is more than a handler could reconstruct.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_slice(&self.body).map_err(|error| error.to_string())
    }

    /// A query parameter, if present and non-empty.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.query
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// A query parameter parsed as an integer, ignoring unparseable values.
    pub fn param_i64(&self, key: &str) -> Option<i64> {
        self.param(key)?.parse().ok()
    }

    /// A query parameter parsed as a bounded count.
    pub fn param_usize(&self, key: &str, max: usize) -> Option<usize> {
        Some(self.param(key)?.parse::<usize>().ok()?.min(max))
    }
}

/// A response ready to be written to any transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Entity tag for a body the browser may reuse after asking, when there is one.
    ///
    /// Present on assets and absent everywhere else, which is the whole caching policy. The assets used to
    /// be sent as `immutable` for a week instead, at a URL carrying no version — so a browser that had
    /// opened the dashboard in the previous week paired a fresh `index.html` with a stale `app.js` and ran
    /// whichever one it had against the other's markup. That is not a theoretical staleness: renaming one
    /// element's id took the entire page down with `Cannot read properties of null`.
    pub etag: Option<String>,
    /// Methods the requested path accepts, on a refusal that exists because it did not accept this one.
    ///
    /// A 405 is required to say what it would have accepted, and the set is per-path now that some paths
    /// take `POST`. Set only by [`Resp::method_not_allowed`]; the transport turns it into the header.
    pub allow: Option<&'static str>,
}

impl Resp {
    /// A JSON body serialised from `value`.
    ///
    /// Serialisation failure becomes a 500 rather than a panic: a malformed row must not take the
    /// dashboard down.
    pub fn json<T: Serialize>(value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self {
                status: 200,
                content_type: "application/json; charset=utf-8",
                body,
                etag: None,
                allow: None,
            },
            Err(error) => Self::error(500, &format!("failed to serialise response: {error}")),
        }
    }

    pub fn html(body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
            etag: None,
            allow: None,
        }
    }

    /// A static asset compiled into the binary, tagged so a browser can ask whether its copy still stands.
    pub fn asset(content_type: &'static str, body: &'static [u8]) -> Self {
        Self {
            status: 200,
            content_type,
            etag: Some(etag(body)),
            body: body.to_vec(),
            allow: None,
        }
    }

    /// A JSON body reporting that work has been started rather than completed.
    ///
    /// `202` rather than `200` because that is exactly what starting a benchmark is: the request has been
    /// accepted and the answer will take between forty-five seconds and a quarter of an hour, so the page
    /// is handed an identifier and told to come back. A `200` would claim the run had finished.
    pub fn accepted<T: Serialize>(value: &T) -> Self {
        let mut response = Self::json(value);
        if response.status == 200 {
            response.status = 202;
        }
        response
    }

    /// A JSON error body, so the dashboard can display a message rather than a blank chart.
    pub fn error(status: u16, message: &str) -> Self {
        let body = serde_json::json!({ "error": message });
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()),
            etag: None,
            allow: None,
        }
    }

    pub fn not_found() -> Self {
        Self::error(404, "not found")
    }

    /// A refusal for a method the requested path does not answer.
    ///
    /// The gate exists while it costs nothing rather than on the day it is needed. Dispatching on the path
    /// alone answered `POST /api/series` as though it were a `GET`, which is harmless only for as long as
    /// every handler is a read — and the day one is not, the hole is in the router rather than in the new
    /// handler, which is the harder place to notice it. That day has now arrived: some paths answer `POST`
    /// and most do not, so the refusal names what *this* path accepts rather than what the server does.
    ///
    /// `allow` is the `Allow` header's value, which the specification requires a 405 to carry. Carried on
    /// the response rather than reconstructed by the transport, because only the router knows which set
    /// applied.
    pub fn method_not_allowed(allow: &'static str) -> Self {
        let mut response = Self::error(
            405,
            &format!("this path answers {allow}, and not the method used"),
        );
        response.allow = Some(allow);
        response
    }
}

/// A short entity tag over an asset's bytes.
///
/// Derived from the content and not from the release version, which is the distinction that matters here.
/// The assets change whenever the binary does, including between two builds of the same version — so a tag
/// naming the version would go stale precisely while someone was editing them, which is where a browser
/// serving yesterday's script is hardest to suspect.
///
/// Eight bytes of SHA-256. This is a cache validator on a loopback server, not a security boundary: it has
/// to change when the bytes do, and nothing is deciding trust on it.
fn etag(body: &[u8]) -> String {
    hex::encode(&Sha256::digest(body)[..8])
}

/// Whether an `If-None-Match` value covers `etag`.
///
/// Quotes and the weak marker are stripped, and the value is treated as the comma-separated list the
/// specification allows, because a browser is entitled to send one. `*` matches any current representation.
pub fn matches_etag(if_none_match: &str, etag: &str) -> bool {
    if_none_match.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.trim_start_matches("W/").trim_matches('"') == etag
    })
}

/// Parse `a=1&b=2`, percent-decoding both sides.
fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

/// Minimal percent-decoding, treating `+` as a space.
///
/// Hand-rolled rather than pulling a dependency: the only inputs are produced by our own dashboard,
/// and invalid escapes are passed through rather than rejected.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(decoded) => {
                        out.push(decoded);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_path_parses_with_no_parameters() {
        let req = Req::parse("/api/live");
        assert_eq!(req.path, "/api/live");
        assert!(req.query.is_empty());
        assert_eq!(req.param("missing"), None);
    }

    #[test]
    fn query_parameters_are_split_and_decoded() {
        let req = Req::parse("/api/series?metric=cpu_percent&from=100&to=200");
        assert_eq!(req.path, "/api/series");
        assert_eq!(req.param("metric"), Some("cpu_percent"));
        assert_eq!(req.param_i64("from"), Some(100));
        assert_eq!(req.param_i64("to"), Some(200));
    }

    #[test]
    fn percent_and_plus_escapes_are_decoded() {
        let req = Req::parse("/x?a=hello%20world&b=one+two&c=%2Fpath%2F");
        assert_eq!(req.param("a"), Some("hello world"));
        assert_eq!(req.param("b"), Some("one two"));
        assert_eq!(req.param("c"), Some("/path/"));
    }

    #[test]
    fn malformed_escapes_pass_through_rather_than_failing() {
        let req = Req::parse("/x?a=100%&b=%zz");
        assert_eq!(req.param("a"), Some("100%"));
        assert_eq!(req.param("b"), Some("%zz"));
    }

    #[test]
    fn empty_and_unparseable_parameters_are_treated_as_absent() {
        let req = Req::parse("/x?a=&b=notanumber&c=5");
        assert_eq!(req.param("a"), None);
        assert_eq!(req.param_i64("b"), None);
        assert_eq!(req.param_i64("c"), Some(5));
    }

    #[test]
    fn bounded_counts_are_clamped() {
        let req = Req::parse("/x?limit=999999");
        assert_eq!(req.param_usize("limit", 1000), Some(1000));
    }

    #[test]
    fn responses_carry_the_expected_status_and_type() {
        let json = Resp::json(&serde_json::json!({ "ok": true }));
        assert_eq!(json.status, 200);
        assert!(json.content_type.starts_with("application/json"));
        assert_eq!(json.body, br#"{"ok":true}"#.to_vec());

        let missing = Resp::not_found();
        assert_eq!(missing.status, 404);
        assert!(String::from_utf8_lossy(&missing.body).contains("not found"));
    }

    /// The tag has to follow the bytes, or a browser keeps yesterday's script for ever.
    #[test]
    fn an_entity_tag_changes_with_the_body() {
        assert_ne!(etag(b"one"), etag(b"two"));
        assert_eq!(etag(b"one"), etag(b"one"));
    }

    /// What browsers actually send back: the tag in quotes, sometimes weak, sometimes several.
    #[test]
    fn a_returned_entity_tag_is_recognised_however_it_is_quoted() {
        assert!(matches_etag("\"abc123\"", "abc123"));
        assert!(matches_etag("W/\"abc123\"", "abc123"));
        assert!(matches_etag("\"other\", \"abc123\"", "abc123"));
        assert!(matches_etag("*", "abc123"));
    }

    /// A tag that does not match must produce the body, not a 304 the browser cannot satisfy.
    #[test]
    fn a_different_entity_tag_does_not_match() {
        assert!(!matches_etag("\"stale\"", "abc123"));
        assert!(!matches_etag("", "abc123"));
        assert!(!matches_etag("\"abc\"", "abc123"));
    }
}
