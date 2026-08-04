//! Static assets compiled into the binary.
//!
//! Embedded rather than fetched from a CDN so the dashboard works with no network at all, consistent
//! with the tool's offline stance, and so a release binary is genuinely self-contained.

use crate::watch::serve::response::Resp;

/// uPlot version vendored under `assets/`. Bump alongside the files.
pub const UPLOT_VERSION: &str = "1.6.32";

const INDEX_HTML: &str = include_str!("../assets/index.html");
const BENCH_HTML: &str = include_str!("../assets/bench.html");
const COMPARE_HTML: &str = include_str!("../assets/compare.html");
const STYLES_CSS: &[u8] = include_bytes!("../assets/styles.css");
const APP_JS: &[u8] = include_bytes!("../assets/app.js");
const BENCH_JS: &[u8] = include_bytes!("../assets/bench.js");
const COMPARE_JS: &[u8] = include_bytes!("../assets/compare.js");
const CHART_JS: &[u8] = include_bytes!("../assets/chart.js");
const FORMAT_JS: &[u8] = include_bytes!("../assets/format.js");
const SERIES_JS: &[u8] = include_bytes!("../assets/series.js");
const UPLOT_JS: &[u8] = include_bytes!("../assets/uplot.min.js");
const UPLOT_CSS: &[u8] = include_bytes!("../assets/uplot.min.css");
const UPLOT_LICENSE: &[u8] = include_bytes!("../assets/uplot.LICENSE");

const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// Every document this dashboard serves, with the scripts each one loads.
///
/// The pairing is the point, and it is checked at `cargo test` rather than in somebody's console: a script
/// looks up its elements by id, so a document and a script that disagree take the whole page down with
/// `Cannot read properties of null`. One list, so a new page cannot be added without declaring what runs on
/// it. `format.js` appears three times because all three pages import it, which is exactly the sharing that
/// makes checking each page separately necessary.
#[cfg(test)]
const PAGES: &[(&str, &str, &[&str])] = &[
    (
        "/",
        INDEX_HTML,
        &[
            include_str!("../assets/app.js"),
            include_str!("../assets/chart.js"),
            include_str!("../assets/series.js"),
            include_str!("../assets/format.js"),
        ],
    ),
    (
        "/bench",
        BENCH_HTML,
        &[
            include_str!("../assets/bench.js"),
            include_str!("../assets/format.js"),
        ],
    ),
    (
        "/compare",
        COMPARE_HTML,
        &[
            include_str!("../assets/compare.js"),
            include_str!("../assets/format.js"),
        ],
    ),
];

/// Resolve a request path to an embedded asset.
///
/// The three documents answer both a bare name and an `.html` one, because a person typing the address of a
/// page they were told about will type either.
pub fn get(path: &str) -> Option<Resp> {
    Some(match path {
        "/" | "/index.html" => Resp::html(INDEX_HTML),
        "/bench" | "/bench.html" => Resp::html(BENCH_HTML),
        "/compare" | "/compare.html" => Resp::html(COMPARE_HTML),
        "/assets/styles.css" => Resp::asset(CSS, STYLES_CSS),
        "/assets/app.js" => Resp::asset(JAVASCRIPT, APP_JS),
        "/assets/bench.js" => Resp::asset(JAVASCRIPT, BENCH_JS),
        "/assets/compare.js" => Resp::asset(JAVASCRIPT, COMPARE_JS),
        "/assets/chart.js" => Resp::asset(JAVASCRIPT, CHART_JS),
        "/assets/format.js" => Resp::asset(JAVASCRIPT, FORMAT_JS),
        "/assets/series.js" => Resp::asset(JAVASCRIPT, SERIES_JS),
        "/assets/uplot.min.js" => Resp::asset(JAVASCRIPT, UPLOT_JS),
        "/assets/uplot.min.css" => Resp::asset(CSS, UPLOT_CSS),
        "/assets/uplot.LICENSE" => Resp::asset("text/plain; charset=utf-8", UPLOT_LICENSE),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::queries::{CondSeries, SampleSeries, SessionSeries};

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
            ("/assets/bench.js", "text/javascript"),
            ("/assets/compare.js", "text/javascript"),
            ("/assets/chart.js", "text/javascript"),
            ("/assets/format.js", "text/javascript"),
            ("/assets/series.js", "text/javascript"),
            ("/assets/styles.css", "text/css"),
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
            "/assets/bench.js",
            "/assets/compare.js",
            "/assets/chart.js",
            "/assets/format.js",
            "/assets/series.js",
            "/assets/styles.css",
            "/assets/uplot.min.js",
            "/assets/uplot.min.css",
        ] {
            let tag = get(path).expect(path).etag.expect(path);
            assert!(tags.insert(tag), "{path} shares a tag with another asset");
        }
    }

    /// Every element a script looks up by id has to exist in the document that loads it.
    ///
    /// This is the contract that broke: a panel's id was renamed in both files at once, which is correct,
    /// and a browser holding a cached copy of one file then ran it against the other. The caching is fixed
    /// separately — but the pairing is checkable here, in the only place that sees both files, and a
    /// mismatch is worth catching at `cargo test` rather than in somebody's console. Deliberately literal:
    /// it reads the ids out of the source text, because there is no JavaScript tooling in this project and
    /// a string search over embedded assets needs none.
    ///
    /// Checked per document, which matters now that there are three. Checking every script against the
    /// union of the markup would pass while `bench.js` looked up an element that only the machine page
    /// defines — the exact failure this test exists to catch, silently readmitted by the convenience of one
    /// combined haystack.
    #[test]
    fn every_id_a_script_looks_up_exists_in_the_document_that_loads_it() {
        let mut checked = 0;
        for (path, document, scripts) in PAGES {
            for script in *scripts {
                for id in referenced_ids(script) {
                    assert!(
                        document.contains(&format!("id=\"{id}\"")),
                        "a script loaded by {path} looks up #{id}, which that document does not define"
                    );
                    checked += 1;
                }
            }
        }
        // The chart catalogue's ids belong to the machine page, whose scripts are listed above.
        for id in referenced_ids(CATALOGUE) {
            assert!(
                INDEX_HTML.contains(&format!("id=\"{id}\"")),
                "the chart catalogue names #{id}, which the machine page does not define"
            );
            checked += 1;
        }
        // A search that silently matched nothing would pass for ever while checking nothing.
        assert!(checked >= 40, "only found {checked} ids to check");
    }

    /// Every page is reachable by both spellings, and each carries the nav that reaches the other two.
    ///
    /// The nav is duplicated in three documents because this project has no template step. That is a
    /// defensible cost only while the duplication is complete: a page missing one link is a dead end a
    /// reader reaches and cannot leave.
    #[test]
    fn every_page_is_served_by_both_names_and_links_to_all_three() {
        for (path, document, _) in PAGES {
            let bare = get(path).unwrap_or_else(|| panic!("{path} should resolve"));
            assert_eq!(bare.status, 200, "{path}");
            assert!(bare.content_type.starts_with("text/html"), "{path}");

            // `/` is spelled `/index.html`; the others take their own name.
            let named = if *path == "/" {
                "/index.html".to_string()
            } else {
                format!("{path}.html")
            };
            let by_name = get(&named).unwrap_or_else(|| panic!("{named} should resolve"));
            assert_eq!(
                by_name.body, bare.body,
                "{named} must serve the same document"
            );

            for link in ["href=\"/\"", "href=\"/bench\"", "href=\"/compare\""] {
                assert!(document.contains(link), "{path} does not carry {link}");
            }
            assert!(
                document.contains("aria-current=\"page\""),
                "{path} does not mark which page it is"
            );
            assert!(
                document.contains("href=\"/assets/styles.css\""),
                "{path} does not load the shared stylesheet"
            );
        }
    }

    /// A document is never served from cache, for the reason recorded on [`the_index_is_never_cached`].
    #[test]
    fn no_document_carries_an_entity_tag() {
        for (path, _, _) in PAGES {
            assert_eq!(
                get(path).unwrap_or_else(|| panic!("{path}")).etag,
                None,
                "{path} must not be cacheable: it names which script to load"
            );
        }
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

    /// The catalogue of measurements the four history switches offer, as authored.
    ///
    /// Read as text for the same reason the id check above is: there is no JavaScript tooling in this
    /// project and none is needed here. The two checks below search for `metric: '…'`, which is the one
    /// field in that file whose spelling has to agree with the server, and the quotes are part of the
    /// pattern so that `output_tokens` cannot pass by matching `output_tokens_per_s`.
    const CATALOGUE: &str = include_str!("../assets/series.js");

    /// Every series the daemon collects is reachable from the page.
    ///
    /// Collection that cannot be read is cost without benefit, and this is the test that makes that stick:
    /// the daemon spent a release advertising thirteen series from `/api/series` against two that were
    /// charted, which nothing failed on because a series nobody can request simply costs disk quietly. A
    /// new variant on any of these three closed enums now fails the build until a frame offers it.
    ///
    /// The three enums only. Probe and `bench:` metrics stay curated — eighteen catalogued metrics from two
    /// sources is a list, not a set of charts — and the judged four are asserted separately, in
    /// `comparison::subjects`, which is where the set that earns a verdict is defined.
    #[test]
    fn every_collected_series_has_a_button_on_the_page() {
        let collected = SampleSeries::ALL
            .iter()
            .map(|series| series.wire_name().to_string())
            .chain(
                SessionSeries::ALL
                    .iter()
                    .map(|series| series.wire_name().to_string()),
            )
            .chain(CondSeries::ALL.iter().map(|series| series.wire_name()));
        let mut checked = 0;
        for name in collected {
            assert!(
                CATALOGUE.contains(&format!("metric: '{name}'")),
                "{name} is collected, so some frame has to offer a chart of it"
            );
            checked += 1;
        }
        // A pattern that silently matched nothing would pass for ever while checking nothing.
        assert!(checked >= 20, "only found {checked} collected series");
    }

    /// Every measurement the page offers is one the server would answer for.
    ///
    /// The other direction, and the cheaper failure to cause: a mistyped name here costs a button that
    /// loads nothing, and the symptom is one empty frame on a page whose frames are legitimately empty for
    /// the first day of collection. Checked against the endpoint's own advertised list, so probe and
    /// `bench:` names are covered by the same assertion as the closed enums.
    #[test]
    fn every_metric_the_page_offers_is_a_series_the_server_answers() {
        let known = crate::watch::serve::handlers::series::known_series();
        let offered = catalogue_metrics();
        assert!(
            offered.len() >= 20,
            "only found {} metrics in the catalogue",
            offered.len()
        );
        for metric in offered {
            assert!(
                known.contains(&metric),
                "the page offers {metric}, which /api/series would reject as unknown"
            );
        }
    }

    /// The `metric: '…'` fields of the catalogue, in the order they are authored.
    fn catalogue_metrics() -> Vec<String> {
        let mut metrics = Vec::new();
        let mut rest = CATALOGUE;
        while let Some(start) = rest.find("metric: '") {
            rest = &rest[start + "metric: '".len()..];
            let Some(end) = rest.find('\'') else { break };
            metrics.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        metrics
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
