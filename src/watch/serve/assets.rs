//! Static assets compiled into the binary.
//!
//! Embedded rather than fetched from a CDN so the dashboard works with no network at all, consistent
//! with the tool's offline stance, and so a release binary is genuinely self-contained.

use crate::watch::serve::response::Resp;

/// uPlot version vendored under `assets/`. Bump alongside the files.
pub const UPLOT_VERSION: &str = "1.6.32";

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &[u8] = include_bytes!("../assets/app.js");
const CHART_JS: &[u8] = include_bytes!("../assets/chart.js");
const FORMAT_JS: &[u8] = include_bytes!("../assets/format.js");
const UPLOT_JS: &[u8] = include_bytes!("../assets/uplot.min.js");
const UPLOT_CSS: &[u8] = include_bytes!("../assets/uplot.min.css");
const UPLOT_LICENSE: &[u8] = include_bytes!("../assets/uplot.LICENSE");

const JAVASCRIPT: &str = "text/javascript; charset=utf-8";

/// Resolve a request path to an embedded asset.
pub fn get(path: &str) -> Option<Resp> {
    Some(match path {
        "/" | "/index.html" => Resp::html(INDEX_HTML),
        "/assets/app.js" => Resp::asset(JAVASCRIPT, APP_JS),
        "/assets/chart.js" => Resp::asset(JAVASCRIPT, CHART_JS),
        "/assets/format.js" => Resp::asset(JAVASCRIPT, FORMAT_JS),
        "/assets/uplot.min.js" => Resp::asset(JAVASCRIPT, UPLOT_JS),
        "/assets/uplot.min.css" => Resp::asset("text/css; charset=utf-8", UPLOT_CSS),
        "/assets/uplot.LICENSE" => Resp::asset("text/plain; charset=utf-8", UPLOT_LICENSE),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_served_at_root_and_by_name() {
        for path in ["/", "/index.html"] {
            let resp = get(path).expect(path);
            assert_eq!(resp.status, 200);
            assert!(resp.content_type.starts_with("text/html"));
        }
    }

    #[test]
    fn vendored_assets_are_present_and_non_trivial() {
        for (path, expected_type) in [
            ("/assets/app.js", "text/javascript"),
            ("/assets/chart.js", "text/javascript"),
            ("/assets/format.js", "text/javascript"),
            ("/assets/uplot.min.js", "text/javascript"),
            ("/assets/uplot.min.css", "text/css"),
        ] {
            let resp = get(path).expect(path);
            assert!(resp.content_type.starts_with(expected_type), "{path}");
            assert!(resp.body.len() > 512, "{path} looks empty");
            assert!(resp.etag.is_some(), "{path} should carry an entity tag");
        }
    }

    /// The index is deliberately untagged, so it is never served from a browser's cache.
    ///
    /// It is the document that names which script to load, and a stale one would defeat the point of
    /// tagging the script at all.
    #[test]
    fn the_index_is_never_cached() {
        assert_eq!(get("/").expect("index").etag, None);
    }

    /// Two bodies must not share a tag, or a browser told one is unchanged would keep the other.
    #[test]
    fn each_asset_gets_its_own_entity_tag() {
        let mut tags = std::collections::BTreeSet::new();
        for path in [
            "/assets/app.js",
            "/assets/chart.js",
            "/assets/format.js",
            "/assets/uplot.min.js",
            "/assets/uplot.min.css",
        ] {
            let tag = get(path).expect(path).etag.expect(path);
            assert!(tags.insert(tag), "{path} shares a tag with another asset");
        }
    }

    /// Every element the scripts look up by id has to exist in the markup they are loaded beside.
    ///
    /// This is the contract that broke: a panel's id was renamed in both files at once, which is correct,
    /// and a browser holding a cached copy of one file then ran it against the other. The caching is fixed
    /// separately — but the pairing is checkable here, in the only place that sees both files, and a
    /// mismatch is worth catching at `cargo test` rather than in somebody's console. Deliberately literal:
    /// it reads the ids out of the source text, because there is no JavaScript tooling in this project and
    /// a string search over two embedded assets needs none.
    #[test]
    fn every_id_the_scripts_look_up_exists_in_the_markup() {
        let scripts = [
            include_str!("../assets/app.js"),
            include_str!("../assets/chart.js"),
            include_str!("../assets/format.js"),
        ];
        let mut checked = 0;
        for script in scripts {
            for id in referenced_ids(script) {
                assert!(
                    INDEX_HTML.contains(&format!("id=\"{id}\"")),
                    "the scripts look up #{id}, which the markup does not define"
                );
                checked += 1;
            }
        }
        // A search that silently matched nothing would pass for ever while checking nothing.
        assert!(checked >= 10, "only found {checked} ids to check");
    }

    /// Ids named in `getElementById('x')` calls and in the `{ id: 'x' }` chart definitions.
    fn referenced_ids(script: &str) -> Vec<String> {
        let mut ids = Vec::new();
        for (prefix, suffix) in [("getElementById('", "')"), ("{ id: '", "'")] {
            let mut rest = script;
            while let Some(start) = rest.find(prefix) {
                rest = &rest[start + prefix.len()..];
                let Some(end) = rest.find(suffix.chars().next().expect("a suffix")) else {
                    break;
                };
                let id = &rest[..end];
                // Only literal ids: anything built at runtime, such as `getElementById(config.id)`, has no
                // fixed name to check and is covered by whatever supplies it.
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    ids.push(id.to_string());
                }
                rest = &rest[end..];
            }
        }
        ids
    }

    /// Vendored third-party code must ship its licence, and it must be the expected one.
    #[test]
    fn the_uplot_licence_is_embedded() {
        let resp = get("/assets/uplot.LICENSE").expect("licence");
        let text = String::from_utf8_lossy(&resp.body);
        assert!(text.contains("MIT"), "{text}");
    }

    #[test]
    fn unknown_and_traversal_paths_resolve_to_nothing() {
        for path in [
            "/assets/../../Cargo.toml",
            "/assets/",
            "/nope.js",
            "/assets/app.js/extra",
        ] {
            assert!(get(path).is_none(), "{path} must not resolve");
        }
    }
}
