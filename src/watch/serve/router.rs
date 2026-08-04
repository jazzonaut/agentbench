//! Path dispatch.
//!
//! Kept separate from the server so the entire routing table can be exercised in unit tests without
//! opening a port.

use crate::watch::{
    serve::{
        Settings, assets,
        handlers::{annotations, live, series, status, verdicts},
        response::{Req, Resp},
    },
    store::Reader,
};

/// Route a request to its handler.
pub fn route(req: &Req, reader: &Reader, settings: &Settings) -> Resp {
    if let Some(asset) = assets::get(&req.path) {
        return asset;
    }
    match req.path.as_str() {
        "/api/live" => live::handle(req, reader),
        "/api/series" => series::handle(req, reader),
        "/api/status" => status::handle(req, reader, settings),
        "/api/verdicts" => verdicts::handle(req, reader, settings.baseline_window_days),
        "/api/annotations" => annotations::handle(req, reader),
        _ => Resp::not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Inventory, watch::store::Store};
    use serde_json::Value;

    struct Fixture {
        temp: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let inventory = Inventory {
                hostname_hash: "hash-router".into(),
                os: "TestOS".into(),
                logical_cores: 4,
                memory_bytes: 8 << 30,
                ..Default::default()
            };
            let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
            Self { temp, store }
        }

        /// Close the store so the writer commits, keeping the directory it wrote to.
        ///
        /// Dropping the whole fixture would take the temporary directory with it. On Windows that
        /// deletion quietly fails while the database file is still open, so a test could drop the
        /// fixture and still reopen the database; on Unix the deletion succeeds and the same test
        /// reopens nothing. Handing the directory back makes the lifetime explicit on both.
        fn close(self) -> tempfile::TempDir {
            let Self { temp, store } = self;
            store.shutdown().expect("the writer should stop cleanly");
            temp
        }

        fn get(&self, target: &str) -> Resp {
            let reader = self.store.reader().unwrap();
            route(&Req::parse(target), &reader, &Settings::default())
        }

        fn json(&self, target: &str) -> Value {
            let resp = self.get(target);
            assert_eq!(resp.status, 200, "{target}: {:?}", body(&resp));
            serde_json::from_slice(&resp.body).expect("valid json")
        }
    }

    fn body(resp: &Resp) -> String {
        String::from_utf8_lossy(&resp.body).into_owned()
    }

    #[test]
    fn every_api_endpoint_answers_with_json() {
        let fixture = Fixture::new();
        for target in [
            "/api/live",
            "/api/series?metric=cpu_percent",
            "/api/series?metric=probe:filesystem.small_file_ops_s",
            "/api/series?metric=probe:filesystem.small_file_ops_s&contended=exclude",
            "/api/status",
            "/api/verdicts",
            "/api/annotations",
        ] {
            let resp = fixture.get(target);
            assert_eq!(resp.status, 200, "{target} -> {}", body(&resp));
            assert!(
                resp.content_type.starts_with("application/json"),
                "{target}"
            );
            serde_json::from_slice::<Value>(&resp.body).expect(target);
        }
    }

    #[test]
    fn the_index_and_assets_are_routed_before_the_api() {
        let fixture = Fixture::new();
        let index = fixture.get("/");
        assert_eq!(index.status, 200);
        assert!(index.content_type.starts_with("text/html"));
        assert!(fixture.get("/assets/uplot.min.js").status == 200);
    }

    #[test]
    fn an_empty_database_reports_no_sample_rather_than_failing() {
        let fixture = Fixture::new();
        let live = fixture.json("/api/live");
        assert!(live["sample"].is_null(), "{live}");
        assert!(live["machine_id"].as_str().is_some());

        assert!(live["probe"].is_null(), "{live}");

        let status = fixture.json("/api/status");
        assert_eq!(status["health"]["samples"], 0);
        assert_eq!(status["health"]["probe_runs"], 0);
        assert_eq!(status["health"]["run_markers"], 0);
        assert_eq!(status["collecting"], false);
        assert!(status["sample_age_ms"].is_null());

        // A machine with no history reaches no verdict, and says which side is missing rather than
        // reporting that everything looks normal.
        let verdicts = fixture.json("/api/verdicts");
        let comparisons = verdicts["comparisons"].as_array().expect("comparisons");
        assert!(!comparisons.is_empty(), "the curated set is not empty");
        for comparison in comparisons {
            assert_eq!(comparison["verdict"], "insufficient", "{comparison}");
            assert!(comparison["baseline"].is_null());
            assert!(
                comparison["note"]
                    .as_str()
                    .is_some_and(|note| !note.is_empty()),
                "{comparison}"
            );
        }
        assert_eq!(verdicts["window_days"], 7);

        assert!(
            fixture.json("/api/annotations")["annotations"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
    }

    /// The endpoints that read a range reject an impossible one rather than guessing.
    #[test]
    fn an_inverted_range_is_a_client_error_on_every_endpoint_that_takes_one() {
        let fixture = Fixture::new();
        for target in [
            "/api/series?metric=cpu_percent&from=500&to=100",
            "/api/annotations?from=500&to=100",
        ] {
            assert_eq!(fixture.get(target).status, 400, "{target}");
        }
    }

    /// A version change and a marked run come back out of the annotations endpoint.
    #[test]
    fn annotations_report_versions_and_marked_runs() {
        let fixture = Fixture::new();
        let now = crate::watch::store::now_ms();
        assert!(fixture.store.sink().send(crate::watch::store::ToolVersion {
            ts: now - 10_000,
            tool: "claude-code".into(),
            version: "2.1.187".into(),
        }));
        assert!(fixture.store.sink().send(crate::watch::store::RunMarker {
            run_id: "run-annotated".into(),
            kind: "benchmark".into(),
            preset: Some("quick".into()),
            started: now - 5_000,
            ended: Some(now - 1_000),
            report_path: Some("D:\\reports\\one.json".into()),
        }));

        let inventory = Inventory {
            hostname_hash: "hash-router".into(),
            ..Default::default()
        };
        let temp = fixture.close();
        let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
        let reader = store.reader().unwrap();
        let resp = route(
            &Req::parse(&format!(
                "/api/annotations?from={}&to={}",
                now - 60_000,
                now
            )),
            &reader,
            &Settings::default(),
        );
        assert_eq!(resp.status, 200, "{}", body(&resp));
        let payload: Value = serde_json::from_slice(&resp.body).unwrap();
        let marks = payload["annotations"].as_array().expect("annotations");
        assert_eq!(marks.len(), 2, "{payload}");
        let kinds: Vec<&str> = marks
            .iter()
            .filter_map(|mark| mark["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"tool_version"), "{kinds:?}");
        assert!(kinds.contains(&"run"), "{kinds:?}");
    }

    /// A probe run travels through the real writer and comes back out of the real endpoints.
    #[test]
    fn a_probe_run_reaches_the_series_endpoint_and_the_live_tiles() {
        let fixture = Fixture::new();
        let now = crate::watch::store::now_ms();
        let probe = |ts: i64, contended: bool, ops: f64| crate::watch::store::ProbeRun {
            ts,
            covariates: crate::watch::store::Covariates {
                cpu_percent: Some(if contended { 90.0 } else { 4.0 }),
                scanner_percent: Some(if contended { 30.0 } else { 0.1 }),
                agent_active: false,
                contended,
                on_battery: Some(false),
            },
            metrics: vec![crate::watch::store::ProbeMetric {
                name: "filesystem.small_file_ops_s".into(),
                value: ops,
                unit: "ops/s".into(),
                lower_is_better: false,
                source: crate::watch::store::MetricSource::Probe,
            }],
        };
        assert!(
            fixture
                .store
                .sink()
                .send(probe(now - 2_000, false, 4_000.0))
        );
        assert!(fixture.store.sink().send(probe(now - 1_000, true, 800.0)));

        let inventory = Inventory {
            hostname_hash: "hash-router".into(),
            ..Default::default()
        };
        let temp = fixture.close();
        let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
        let reader = store.reader().unwrap();
        let json = |target: &str| -> Value {
            let resp = route(&Req::parse(target), &reader, &Settings::default());
            assert_eq!(resp.status, 200, "{target}: {}", body(&resp));
            serde_json::from_slice(&resp.body).unwrap()
        };

        let all = json("/api/series?metric=probe:filesystem.small_file_ops_s");
        assert_eq!(all["points"].as_array().map(Vec::len), Some(2), "{all}");
        assert_eq!(all["unit"], "ops/s", "the catalogue supplies the unit");
        assert_eq!(all["lower_is_better"], false);

        let clean = json("/api/series?metric=probe:filesystem.small_file_ops_s&contended=exclude");
        let points = clean["points"].as_array().expect("points");
        assert_eq!(points.len(), 1, "the contended run is excluded: {clean}");
        assert_eq!(points[0]["value"].as_f64(), Some(4_000.0));

        // The full-scale source is a different series and holds nothing.
        let bench = json("/api/series?metric=bench:filesystem.small_file_ops_s");
        assert!(bench["points"].as_array().expect("points").is_empty());

        let live = json("/api/live");
        assert_eq!(live["probe"]["contended"], true, "the newest run: {live}");
        assert_eq!(live["probe"]["metrics"], 1);

        let status = json("/api/status");
        assert_eq!(status["health"]["probe_runs"], 2);
        assert_eq!(status["health"]["probe_runs_clean"], 1);
    }

    /// A run marker goes in twice and stays one row, through the real writer.
    #[test]
    fn a_run_marker_is_recorded_once_and_counted() {
        let fixture = Fixture::new();
        let marker = |ended: Option<i64>| crate::watch::store::RunMarker {
            run_id: "run-router".into(),
            kind: "benchmark".into(),
            preset: Some("quick".into()),
            started: 1_700_000_000_000,
            ended,
            report_path: None,
        };
        assert!(fixture.store.sink().send(marker(None)));
        assert!(fixture.store.sink().send(marker(Some(1_700_000_180_000))));

        let inventory = Inventory {
            hostname_hash: "hash-router".into(),
            ..Default::default()
        };
        let temp = fixture.close();
        let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
        let reader = store.reader().unwrap();

        let resp = route(&Req::parse("/api/status"), &reader, &Settings::default());
        let status: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(status["health"]["run_markers"], 1, "{status}");
    }

    #[test]
    fn status_reports_collecting_once_a_sample_lands() {
        let fixture = Fixture::new();
        fixture.store.sink().send(crate::watch::store::Sample {
            ts: crate::watch::store::now_ms(),
            cpu_percent: 21.5,
            used_memory: 1 << 30,
            total_memory: 8 << 30,
            used_swap: 0,
            process_count: 300,
            scanner_cpu: None,
            agent_cpu: Some(4.0),
            agent_rss: Some(1 << 28),
            agent_processes: Some(2),
        });
        // Force the writer to commit by closing the store, then reopen for reading.
        let inventory = Inventory {
            hostname_hash: "hash-router".into(),
            ..Default::default()
        };
        let temp = fixture.close();
        let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
        let reader = store.reader().unwrap();

        let resp = route(&Req::parse("/api/status"), &reader, &Settings::default());
        let status: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(status["health"]["samples"], 1);
        assert_eq!(status["collecting"], true);

        let resp = route(&Req::parse("/api/live"), &reader, &Settings::default());
        let live: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(live["sample"]["process_count"], 300);
    }

    /// A quiet machine on a slow idle cadence is between samples, not stalled.
    ///
    /// `sample_interval_idle` reaches minutes legitimately, and against the fixed two-minute threshold this
    /// replaced, a healthy daemon four minutes into a six-minute idle gap reported `collecting: false` and
    /// the page drew `stalled` beside a warning dot.
    #[test]
    fn a_sample_older_than_two_minutes_is_not_stalled_at_a_slow_idle_cadence() {
        let fixture = Fixture::new();
        let four_minutes_ago = crate::watch::store::now_ms() - 4 * 60 * 1000;
        fixture.store.sink().send(crate::watch::store::Sample {
            ts: four_minutes_ago,
            cpu_percent: 1.5,
            used_memory: 1 << 30,
            total_memory: 8 << 30,
            used_swap: 0,
            process_count: 300,
            scanner_cpu: None,
            agent_cpu: None,
            agent_rss: None,
            agent_processes: None,
        });
        let inventory = Inventory {
            hostname_hash: "hash-router".into(),
            ..Default::default()
        };
        let temp = fixture.close();
        let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
        let reader = store.reader().unwrap();

        let judge = |idle: std::time::Duration| -> Value {
            let settings = Settings {
                idle_interval: idle,
                ..Settings::default()
            };
            let resp = route(&Req::parse("/api/status"), &reader, &settings);
            serde_json::from_slice(&resp.body).unwrap()
        };

        assert_eq!(
            judge(std::time::Duration::from_secs(360))["collecting"],
            true,
            "four minutes into a six-minute cadence is a quiet machine"
        );
        assert_eq!(
            judge(std::time::Duration::from_secs(30))["collecting"],
            false,
            "at the shipped cadence four minutes really is a stall"
        );
    }

    /// A fresh sample and a dead writer is a stalled daemon, not a working one.
    ///
    /// This is the shape of the fault the status payload used to be unable to describe: the rows are
    /// recent, so age alone says "collecting", while nothing more will ever be written.
    #[test]
    fn a_stopped_writer_is_reported_and_is_not_read_as_collecting() {
        let fixture = Fixture::new();
        fixture.store.sink().send(crate::watch::store::Sample {
            ts: crate::watch::store::now_ms(),
            cpu_percent: 21.5,
            used_memory: 1 << 30,
            total_memory: 8 << 30,
            used_swap: 0,
            process_count: 300,
            scanner_cpu: None,
            agent_cpu: None,
            agent_rss: None,
            agent_processes: None,
        });
        let health = fixture.store.writer_health();
        let inventory = Inventory {
            hostname_hash: "hash-router".into(),
            ..Default::default()
        };
        let temp = fixture.close();
        assert!(!health.is_running(), "the writer has been shut down");

        let store = Store::open(&temp.path().join("watch.db"), &inventory).unwrap();
        let reader = store.reader().unwrap();
        let settings = Settings::default().watching(health);
        let resp = route(&Req::parse("/api/status"), &reader, &settings);
        let status: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(status["writer_running"], false, "{status}");
        assert_eq!(status["collecting"], false, "{status}");

        // Without a writer to ask about, the field says so rather than claiming health.
        let resp = route(&Req::parse("/api/status"), &reader, &Settings::default());
        let status: Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(status["writer_running"].is_null(), "{status}");
        assert_eq!(status["collecting"], true, "{status}");
    }

    #[test]
    fn a_bad_metric_name_is_a_client_error_listing_the_valid_ones() {
        let fixture = Fixture::new();
        let resp = fixture.get("/api/series?metric=not_a_metric");
        assert_eq!(resp.status, 400);
        let text = body(&resp);
        assert!(text.contains("unknown metric"), "{text}");
        assert!(text.contains("cpu_percent"), "{text}");
    }

    #[test]
    fn a_missing_metric_and_an_inverted_range_are_client_errors() {
        let fixture = Fixture::new();
        assert_eq!(fixture.get("/api/series").status, 400);
        assert_eq!(
            fixture
                .get("/api/series?metric=cpu_percent&from=500&to=100")
                .status,
            400
        );
    }

    #[test]
    fn unknown_paths_are_not_found() {
        let fixture = Fixture::new();
        for target in ["/nope", "/api/", "/api/unknown", "/../Cargo.toml"] {
            assert_eq!(fixture.get(target).status, 404, "{target}");
        }
    }
}
