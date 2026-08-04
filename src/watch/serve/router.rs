//! Path dispatch.
//!
//! Kept separate from the server so the entire routing table can be exercised in unit tests without
//! opening a port.

use crate::watch::{
    serve::{
        Settings, assets,
        handlers::{annotations, bench, compare, live, series, status, today, verdicts},
        response::{Method, Req, Resp},
    },
    store::Reader,
};

/// Methods a read-only path accepts.
const READ_ONLY: &str = "GET, HEAD";

/// Methods a path that only acts accepts.
const WRITE_ONLY: &str = "POST";

/// Route a request to its handler.
///
/// Dispatch is on the pair, not on the path: most paths here are reads and a few are not, so answering
/// `POST /api/series` as though it were a `GET` — which dispatching on the path alone did — is a hole that
/// grows quietly as write endpoints are added. A path that exists but does not answer this method gets a
/// `405` naming what it does answer, which is a different thing from the `404` an unknown path gets.
///
/// The gate that matters for the writes is not here. This decides whether a path *can* act; whether the
/// request is entitled to make it act is settled before routing, in [`super::origin::is_same_origin_write`].
pub fn route(req: &Req, reader: &Reader, settings: &Settings) -> Resp {
    // Assets are reads and there are many of them, so they are matched first and as a group.
    if let Some(asset) = assets::get(&req.path) {
        return if req.method.is_read() {
            asset
        } else {
            Resp::method_not_allowed(READ_ONLY)
        };
    }
    let read = req.method.is_read();
    let post = req.method == Method::Post;
    match req.path.as_str() {
        "/api/live" if read => live::handle(req, reader),
        "/api/today" if read => today::handle(req, reader),
        "/api/series" if read => series::handle(req, reader),
        "/api/status" if read => status::handle(req, reader, settings),
        "/api/verdicts" if read => verdicts::handle(req, reader, settings.baseline_window_days),
        "/api/annotations" if read => annotations::handle(req, reader),
        "/api/bench/options" if read => bench::options(req, settings),
        "/api/bench/run" if read => bench::run(req, settings),
        "/api/bench" if post => bench::start(req, settings),
        "/api/bench/cancel" if post => bench::cancel(req, settings),
        "/api/compare" if post => compare::handle(req),
        // The path exists; the method is what was wrong with the request. Listed separately from the arms
        // above rather than folded into them, so that adding an endpoint above without adding it here is a
        // `404` on a path that works — noisy and immediate — rather than a silent method hole.
        "/api/live" | "/api/today" | "/api/series" | "/api/status" | "/api/verdicts"
        | "/api/annotations" | "/api/bench/options" | "/api/bench/run" => {
            Resp::method_not_allowed(READ_ONLY)
        }
        "/api/bench" | "/api/bench/cancel" | "/api/compare" => Resp::method_not_allowed(WRITE_ONLY),
        _ => Resp::not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::Inventory,
        watch::{serve::response::Method, store::Store},
    };
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

    /// An otherwise-empty benchmark report, for the endpoints that take reports as documents.
    fn fixture_report(schema_version: u32) -> crate::model::Report {
        crate::model::Report {
            schema_version,
            tool_version: "0.0.0-test".into(),
            run_id: "0123456789abcdef".into(),
            created_at: chrono::Utc::now(),
            kind: crate::model::RunKind::Benchmark,
            inventory: Default::default(),
            config: Default::default(),
            metrics: Vec::new(),
            samples: Vec::new(),
            profiles: Vec::new(),
            llm_runs: Vec::new(),
            integrations: Vec::new(),
            findings: Vec::new(),
            warnings: Vec::new(),
            unavailable: Vec::new(),
        }
    }

    #[test]
    fn every_api_endpoint_answers_with_json() {
        let fixture = Fixture::new();
        for target in [
            "/api/live",
            "/api/today",
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

        // A day with no imported transcripts is a day of zeroes, not a missing payload: the tiles read
        // "No agent activity" from the turn count, and the day they say it about comes from the server.
        let today = fixture.json("/api/today");
        assert!(today["day_start_ts"].as_i64().is_some(), "{today}");
        assert_eq!(today["today"]["turns"], 0, "{today}");
        assert!(today["today"]["last_activity_ts"].is_null(), "{today}");

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
                agent_percent: Some(if contended { 250.0 } else { 1.0 }),
                agent_active: false,
                contended,
                on_battery: Some(false),
                clock_percent: Some(if contended { 128.0 } else { 136.0 }),
                disk_write_bytes_s: Some(if contended { 45.0e6 } else { 64.0e3 }),
                scratch_free_bytes: Some(110 << 30),
            },
            processes: Vec::new(),
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
            agent_write_bytes_s: Some(2_097_152.0),
            scanner_write_bytes_s: Some(131_072.0),
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
            agent_write_bytes_s: None,
            scanner_write_bytes_s: None,
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
            agent_write_bytes_s: None,
            scanner_write_bytes_s: None,
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

    /// A method a path does not answer is refused, and the refusal names what that path does answer.
    ///
    /// This rule replaced a blunter one. While every endpoint was a read, the method could be checked
    /// before the path and anything but a `GET` or `HEAD` refused with `405` — including on a path that did
    /// not exist, which advertised `Allow: GET, HEAD` for nothing. Now that some paths answer `POST`, the
    /// allowed set is a property of the path, and a path that does not exist has no such set to report: it
    /// is a `404`, as it is for a read.
    #[test]
    fn a_method_a_path_does_not_answer_is_refused_with_the_set_that_path_accepts() {
        let fixture = Fixture::new();
        let reader = fixture.store.reader().unwrap();
        let answer = |method: Method, target: &str| -> Resp {
            let req = Req::parse(target).with_method(method);
            route(&req, &reader, &Settings::default())
        };

        // Read-only paths, including the assets: a write is refused and told so.
        for target in ["/", "/bench", "/assets/app.js", "/api/live", "/api/status"] {
            for method in [Method::Post, Method::Other] {
                let refused = answer(method, target);
                assert_eq!(refused.status, 405, "{target}: {}", body(&refused));
                assert_eq!(refused.allow, Some("GET, HEAD"), "{target}");
            }
            assert_ne!(answer(Method::Get, target).status, 405, "{target}");
        }

        // Paths that only act: a read is refused, and told that a POST is what they take.
        for target in ["/api/bench", "/api/bench/cancel", "/api/compare"] {
            for method in [Method::Get, Method::Head, Method::Other] {
                let refused = answer(method, target);
                assert_eq!(refused.status, 405, "{target}: {}", body(&refused));
                assert_eq!(refused.allow, Some("POST"), "{target}");
            }
        }

        // A path that does not exist has no allowed set to advertise, whatever the method.
        for method in [Method::Get, Method::Post, Method::Other] {
            let missing = answer(method, "/api/nonsense");
            assert_eq!(missing.status, 404, "{}", body(&missing));
            assert_eq!(missing.allow, None);
        }

        // A HEAD is a GET whose body the transport drops, so the router must answer it in full.
        let head = answer(Method::Head, "/api/live");
        assert_eq!(head.status, 200, "{}", body(&head));
        assert!(!head.body.is_empty(), "the handler ran");
    }

    /// The endpoints that act say so when this daemon has no registry behind them.
    ///
    /// `Settings::default()` has none, which is every test and also a daemon whose `watch.toml` turned runs
    /// off. The distinction that matters is between "this daemon will not" and "this daemon cannot": a 503
    /// with a reason is the first, and it is what the page needs in order to explain itself.
    #[test]
    fn the_run_endpoints_refuse_with_a_reason_when_no_registry_is_present() {
        let fixture = Fixture::new();
        let reader = fixture.store.reader().unwrap();
        let post = |target: &str, body: &str| -> Resp {
            route(&Req::post(target, body), &reader, &Settings::default())
        };

        for target in ["/api/bench", "/api/bench/cancel"] {
            let refused = post(target, "{}");
            assert_eq!(refused.status, 503, "{target}: {}", body(&refused));
            assert!(
                body(&refused).contains("allow_runs"),
                "{target} should name the setting: {}",
                body(&refused)
            );
        }

        // The two reads still answer, so the page renders a disabled form rather than failing to load.
        let options = fixture.json("/api/bench/options");
        assert_eq!(options["allowed"], false, "{options}");
        assert!(options["refusal"].as_str().is_some_and(|r| !r.is_empty()));
        // Every preset is still described, because the form still draws itself.
        assert_eq!(options["presets"].as_array().map(Vec::len), Some(3));

        let run = fixture.json("/api/bench/run");
        assert_eq!(run["state"], "idle", "{run}");
    }

    /// Two reports in, one comparison out — and a refusal that repeats the reason verbatim.
    #[test]
    fn the_compare_endpoint_computes_deltas_and_refuses_an_incomparable_pair() {
        let fixture = Fixture::new();
        let reader = fixture.store.reader().unwrap();
        let compare = |baseline: &Value, candidate: &Value| -> Resp {
            let payload = serde_json::json!({ "baseline": baseline, "candidate": candidate });
            route(
                &Req::post("/api/compare", serde_json::to_vec(&payload).unwrap()),
                &reader,
                &Settings::default(),
            )
        };

        // Serialised from the real types rather than hand-written, so a field added to `Report` cannot leave
        // this test asserting things about a document the server would reject for a different reason.
        let report = |preset: &str, value: f64| -> Value {
            let mut report = fixture_report(crate::SCHEMA_VERSION);
            report.config.preset = Some(preset.to_string());
            report.metrics = vec![crate::model::Metric::scalar(
                "cpu.single_mops_s",
                value,
                "Mops/s",
                false,
                "cpu",
            )];
            serde_json::to_value(&report).expect("a report serialises")
        };

        let resp = compare(&report("standard", 100.0), &report("standard", 75.0));
        assert_eq!(resp.status, 200, "{}", body(&resp));
        let comparison: Value = serde_json::from_slice(&resp.body).unwrap();
        let delta = &comparison["metrics"][0];
        assert_eq!(delta["baseline"], 100.0);
        assert_eq!(delta["candidate"], 75.0);
        assert_eq!(delta["change_percent"], -25.0);
        assert_eq!(delta["interpretation"], "regression", "{comparison}");
        // No path anywhere in the payload: the reports arrived as documents.
        assert_eq!(comparison["preset"], "standard");

        // The compatibility gate's own sentence is what the page displays.
        let refused = compare(&report("quick", 1.0), &report("standard", 1.0));
        assert_eq!(refused.status, 400);
        assert!(
            body(&refused).contains("presets differ"),
            "{}",
            body(&refused)
        );

        // A body that is not two reports is a client error naming what was wrong.
        let malformed = route(
            &Req::post("/api/compare", "{\"baseline\":1}"),
            &reader,
            &Settings::default(),
        );
        assert_eq!(malformed.status, 400);
        assert!(
            body(&malformed).contains("unreadable report"),
            "{}",
            body(&malformed)
        );
    }

    /// A report from a schema this binary does not know is refused, and the message says which file.
    #[test]
    fn a_report_from_another_schema_is_refused_by_name() {
        let fixture = Fixture::new();
        let reader = fixture.store.reader().unwrap();
        let report = |schema: u32| {
            serde_json::to_value(fixture_report(schema)).expect("a report serialises")
        };
        let payload = serde_json::json!({
            "baseline": report(crate::SCHEMA_VERSION),
            "candidate": report(crate::SCHEMA_VERSION + 1),
        });
        let resp = route(
            &Req::post("/api/compare", serde_json::to_vec(&payload).unwrap()),
            &reader,
            &Settings::default(),
        );
        assert_eq!(resp.status, 400);
        let text = body(&resp);
        assert!(text.contains("candidate"), "{text}");
        assert!(text.contains("unsupported report schema"), "{text}");
    }

    #[test]
    fn unknown_paths_are_not_found() {
        let fixture = Fixture::new();
        for target in ["/nope", "/api/", "/api/unknown", "/../Cargo.toml"] {
            assert_eq!(fixture.get(target).status, 404, "{target}");
        }
    }
}
