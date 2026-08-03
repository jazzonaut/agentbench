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
            assert!(resp.immutable, "{path} should be cacheable");
        }
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
