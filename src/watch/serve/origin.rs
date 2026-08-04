//! Whether a request was addressed to this daemon, or merely delivered to it.
//!
//! Binding to `127.0.0.1` stops a *network* peer. It does not stop a browser. Any page the user
//! visits can resolve a name it controls to `127.0.0.1`, wait out the DNS cache, and then reach every
//! endpoint here same-origin — reading real project paths, branch names, models, token counts and the
//! operational log, which is precisely what the loopback restriction exists to keep local. That is DNS
//! rebinding, and the `Host` header is the one part of such a request that still carries the
//! attacker's name rather than ours.
//!
//! So the rule is an allow-list of the names this socket is actually reachable under, and anything
//! else is answered with 421 rather than served. Cross-origin `fetch` is already blocked by the
//! absence of `Access-Control-Allow-Origin`; this closes the remaining route.

/// Whether `host` names this server's own loopback socket.
///
/// A missing header fails: only HTTP/1.0 omits it, no browser does, and there is nothing to gain from
/// guessing on behalf of a client that will not say who it was calling.
pub fn is_own_host(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let (name, host_port) = split(host.trim());
    let port_matches = match host_port {
        Some(value) => value == port,
        // A browser omits the port only when it is the scheme's default, which for `http` is 80.
        None => port == 80,
    };
    port_matches
        && matches!(
            name.to_ascii_lowercase().as_str(),
            "127.0.0.1" | "::1" | "localhost"
        )
}

/// Whether a request that could *change* something came from this dashboard's own pages.
///
/// [`is_own_host`] is not enough for these, and the difference matters. That check refuses a request
/// addressed to somebody else's name; it cannot refuse one addressed correctly to `127.0.0.1:7878` by a
/// page the user happened to have open — a form on `evil.example` submitting here sends exactly the
/// `Host` this server expects. For a read that was acceptable, on the reasoning recorded in ADR 0001:
/// anything reaching loopback could already read the database file. It is not acceptable for a request
/// that starts a benchmark, because a benchmark loads the machine for up to a quarter of an hour and, if
/// it were asked to include live-LLM cases, would spend the user's own API credit doing it.
///
/// Three conditions, all required, none of them sufficient alone:
///
/// - **`Sec-Fetch-Site` is `same-origin`.** The browser states the relationship itself and a page cannot
///   forge it. Absence is tolerated only because `curl` and this crate's own tests send no fetch
///   metadata; every browser released since 2020 does.
/// - **`Origin`, when present, is one of this socket's own names.** A cross-site `fetch` is required to
///   send it, so a mismatch is proof rather than suspicion.
/// - **`Content-Type` is `application/json`.** This is what closes the form route: an HTML form can
///   `POST` cross-site without a preflight, but only as `application/x-www-form-urlencoded`,
///   `multipart/form-data`, or `text/plain`. Requiring JSON makes every write a request the browser must
///   preflight, and a preflight this server never answers.
pub fn is_same_origin_write(
    fetch_site: Option<&str>,
    origin: Option<&str>,
    content_type: Option<&str>,
    port: u16,
) -> bool {
    let site_ok = match fetch_site {
        Some(value) => value.trim().eq_ignore_ascii_case("same-origin"),
        None => true,
    };
    let origin_ok = match origin {
        // `null` is what a sandboxed or `file://` document sends. It names no host, so it cannot be shown
        // to be this one.
        Some(value) => origin_is_own(value.trim(), port),
        None => true,
    };
    let type_ok = content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
    });
    site_ok && origin_ok && type_ok
}

/// Whether an `Origin` header names this server's own loopback socket.
///
/// Only `http` is accepted, because that is the only scheme this server speaks: an `https://127.0.0.1:7878`
/// origin did not come from a page this dashboard served.
fn origin_is_own(origin: &str, port: u16) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|authority| is_own_host(Some(authority), port))
}

/// Split `host[:port]` into its parts, unwrapping the brackets IPv6 literals are written in.
///
/// A port that is present but unparseable yields `Some` of nothing rather than falling back to "no
/// port", so `localhost:notaport` cannot match a server on port 80.
fn split(host: &str) -> (&str, Option<u16>) {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((inside, "")) => (inside, None),
            Some((inside, tail)) => (inside, tail.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (host, None),
        };
    }
    match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port.parse().unwrap_or(0))),
        None => (host, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_addresses_this_server_is_reachable_under_are_accepted() {
        for host in [
            "127.0.0.1:7878",
            "localhost:7878",
            "LOCALHOST:7878",
            "[::1]:7878",
            " 127.0.0.1:7878 ",
        ] {
            assert!(is_own_host(Some(host), 7878), "{host}");
        }
    }

    /// The rebinding case: the request arrives on loopback carrying somebody else's name.
    #[test]
    fn a_name_that_merely_resolves_here_is_refused() {
        for host in [
            "rebind.attacker.example:7878",
            "127.0.0.1.nip.io:7878",
            "localhost.attacker.example:7878",
            "attacker.example",
        ] {
            assert!(!is_own_host(Some(host), 7878), "{host}");
        }
    }

    /// Another daemon's port is another daemon's business, even on the same machine.
    #[test]
    fn the_port_has_to_match_too() {
        assert!(!is_own_host(Some("127.0.0.1:9999"), 7878));
        assert!(!is_own_host(Some("[::1]:9999"), 7878));
        // Omitting the port claims port 80, which this server is not on.
        assert!(!is_own_host(Some("127.0.0.1"), 7878));
        assert!(is_own_host(Some("127.0.0.1"), 80));
        // An unreadable port is not an absent one.
        assert!(!is_own_host(Some("localhost:notaport"), 80));
    }

    #[test]
    fn a_request_that_names_no_host_is_refused() {
        assert!(!is_own_host(None, 7878));
        assert!(!is_own_host(Some(""), 7878));
    }

    /// What the dashboard's own pages send when they start a benchmark.
    #[test]
    fn a_write_from_this_dashboards_own_page_is_accepted() {
        assert!(is_same_origin_write(
            Some("same-origin"),
            Some("http://127.0.0.1:7878"),
            Some("application/json"),
            7878
        ));
        // A charset parameter is part of the header a `fetch` may send, and does not change the essence.
        assert!(is_same_origin_write(
            Some("same-origin"),
            Some("http://localhost:7878"),
            Some("application/json; charset=utf-8"),
            7878
        ));
        // `curl` and this crate's tests send no fetch metadata and no origin.
        assert!(is_same_origin_write(
            None,
            None,
            Some("application/json"),
            7878
        ));
    }

    /// The attack this gate exists for: a page the user visited, posting here with a correct `Host`.
    #[test]
    fn a_write_from_somebody_elses_page_is_refused() {
        // The browser says so itself.
        assert!(!is_same_origin_write(
            Some("cross-site"),
            Some("http://evil.example"),
            Some("application/json"),
            7878
        ));
        // Even without fetch metadata, the origin gives it away.
        assert!(!is_same_origin_write(
            None,
            Some("http://evil.example"),
            Some("application/json"),
            7878
        ));
        // `same-site` is not `same-origin`: a different port on localhost is a different origin.
        assert!(!is_same_origin_write(
            Some("same-site"),
            None,
            Some("application/json"),
            7878
        ));
        // A page navigated to by the user is not a page of ours making a request.
        for site in ["cross-site", "same-site", "none"] {
            assert!(
                !is_same_origin_write(Some(site), None, Some("application/json"), 7878),
                "{site}"
            );
        }
    }

    /// The form route: a cross-site `POST` a browser will send without asking permission first.
    #[test]
    fn a_write_that_is_not_json_is_refused_whatever_else_it_carries() {
        for content_type in [
            None,
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
            Some("text/plain"),
            Some("text/plain;charset=UTF-8"),
            // Close enough to fool a `starts_with`, and not the same media type.
            Some("application/json-patch+json"),
        ] {
            assert!(
                !is_same_origin_write(Some("same-origin"), None, content_type, 7878),
                "{content_type:?}"
            );
        }
    }

    /// An origin that resolves here is still not this origin — the rebinding case, for writes.
    #[test]
    fn an_origin_that_merely_resolves_here_is_refused() {
        for origin in [
            "http://127.0.0.1.nip.io:7878",
            "http://rebind.attacker.example:7878",
            // Another daemon's port, and this daemon's port under a scheme it does not speak.
            "http://127.0.0.1:9999",
            "https://127.0.0.1:7878",
            // A sandboxed or `file://` document names no host at all.
            "null",
        ] {
            assert!(
                !is_same_origin_write(None, Some(origin), Some("application/json"), 7878),
                "{origin}"
            );
        }
    }
}
