//! Transport-independent request and response types.
//!
//! Handlers take a [`Req`] and return a [`Resp`], so they are ordinary functions testable without
//! binding a socket, and `tiny_http` appears in exactly one file.

use serde::Serialize;
use std::collections::BTreeMap;

/// A parsed request, reduced to what handlers actually need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Req {
    /// Path with the query string removed, e.g. `/api/series`.
    pub path: String,
    /// Decoded query parameters.
    pub query: BTreeMap<String, String>,
}

impl Req {
    /// Split a raw request target into path and decoded query parameters.
    pub fn parse(target: &str) -> Self {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Self {
            path: path.to_string(),
            query: parse_query(query),
        }
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
    /// Whether the body may be cached by the browser.
    pub immutable: bool,
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
                immutable: false,
            },
            Err(error) => Self::error(500, &format!("failed to serialise response: {error}")),
        }
    }

    pub fn html(body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
            immutable: false,
        }
    }

    /// A vendored static asset, safe to cache because it only changes with the binary.
    pub fn asset(content_type: &'static str, body: &'static [u8]) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_vec(),
            immutable: true,
        }
    }

    /// A JSON error body, so the dashboard can display a message rather than a blank chart.
    pub fn error(status: u16, message: &str) -> Self {
        let body = serde_json::json!({ "error": message });
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()),
            immutable: false,
        }
    }

    pub fn not_found() -> Self {
        Self::error(404, "not found")
    }
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
}
