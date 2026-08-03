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
}
