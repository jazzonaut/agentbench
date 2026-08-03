//! Path dispatch.
//!
//! Kept separate from the server so the entire routing table can be exercised in unit tests without
//! opening a port.

use crate::watch::{
    serve::{
        assets,
        handlers::{live, series, status},
        response::{Req, Resp},
    },
    store::Reader,
};

/// Route a request to its handler.
pub fn route(req: &Req, reader: &Reader) -> Resp {
    if let Some(asset) = assets::get(&req.path) {
        return asset;
    }
    match req.path.as_str() {
        "/api/live" => live::handle(req, reader),
        "/api/series" => series::handle(req, reader),
        "/api/status" => status::handle(req, reader),
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
            route(&Req::parse(target), &reader)
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
        for target in ["/api/live", "/api/series?metric=cpu_percent", "/api/status"] {
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

        let status = fixture.json("/api/status");
        assert_eq!(status["health"]["samples"], 0);
        assert_eq!(status["collecting"], false);
        assert!(status["sample_age_ms"].is_null());
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

        let resp = route(&Req::parse("/api/status"), &reader);
        let status: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(status["health"]["samples"], 1);
        assert_eq!(status["collecting"], true);

        let resp = route(&Req::parse("/api/live"), &reader);
        let live: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(live["sample"]["process_count"], 300);
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
