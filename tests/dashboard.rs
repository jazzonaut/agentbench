//! End-to-end checks for the `dashboard` daemon.
//!
//! These exercise what unit tests cannot: that the binary starts, the threads wire together, the
//! database is created and migrated on real files, the HTTP server binds and answers, and Ctrl+C-style
//! shutdown leaves committed data behind.
//!
//! Every wait is a poll on an observable condition rather than a fixed sleep, so the tests do not
//! become flaky on a slow or loaded CI runner.

use assert_cmd::cargo::CommandCargoExt;
use std::{
    io::Read,
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Upper bound on how long any single condition may take to become true.
const TIMEOUT: Duration = Duration::from_secs(45);

/// A daemon child process that is always killed, even if a test panics.
struct Daemon {
    child: Child,
    port: u16,
    data_dir: tempfile::TempDir,
}

impl Daemon {
    /// Start with transcript importing switched off.
    ///
    /// What most tests want: the transcript directory of whoever runs the suite is somebody's real
    /// work, not a fixture, and importing hundreds of megabytes of it would be both slow and rude.
    ///
    /// Probing is off too. It is a second of real CPU and disk work every interval, which on a shared
    /// runner would slow every other test in the suite for no benefit to any of them; the one test that
    /// wants probes asks for them.
    fn start(extra: &[&str]) -> Self {
        Self::spawn(extra, &["--no-sessions", "--no-probes"])
    }

    /// Start probing on the shortest permitted interval, with no outbound requests.
    ///
    /// `--no-probe-network` is not politeness — it is what keeps the test from depending on the internet,
    /// and therefore from failing on a runner with no egress for reasons that have nothing to do with the
    /// code under test.
    fn start_probing() -> Self {
        Self::spawn(
            &["--probe-interval", "1s", "--no-probe-network"],
            &["--no-sessions"],
        )
    }

    /// Start importing transcripts from `root`, and from nowhere else.
    fn start_with_transcripts(root: &Path) -> Self {
        Self::spawn(
            &[],
            &["--sessions-root", root.to_str().expect("utf-8 path")],
        )
    }

    /// Start against a data directory that already holds a database.
    ///
    /// The only way to test anything that depends on history: a verdict needs days of it and retention needs
    /// samples older than its window, and neither can be produced by waiting.
    fn start_in(data_dir: tempfile::TempDir, extra: &[&str]) -> Self {
        Self::spawn_in(data_dir, extra, &["--no-sessions", "--no-probes"])
    }

    fn spawn(extra: &[&str], sessions: &[&str]) -> Self {
        Self::spawn_in(tempfile::tempdir().expect("temp data dir"), extra, sessions)
    }

    fn spawn_in(data_dir: tempfile::TempDir, extra: &[&str], sessions: &[&str]) -> Self {
        // Port 0 is not usable here because the daemon prints its URL rather than exposing it, so
        // pick a free port up front and hand it over.
        let port = free_port();
        let mut command = Command::cargo_bin("agentbench").expect("built binary");
        command
            .arg("dashboard")
            .arg("--port")
            .arg(port.to_string())
            .arg("--data-dir")
            .arg(data_dir.path())
            // Sample fast so the first observation lands promptly.
            .arg("--sample-interval")
            .arg("200ms")
            .args(sessions)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("spawn daemon");
        Self {
            child,
            port,
            data_dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn data_dir(&self) -> &Path {
        self.data_dir.path()
    }

    /// Block until the HTTP port accepts connections.
    fn wait_until_listening(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon never bound port {}", self.port);
    }

    /// Fetch a path, retrying until it returns a body.
    fn get(&self, path: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        let mut last = String::new();
        while Instant::now() < deadline {
            match fetch(&self.url(path)) {
                Ok(body) => return body,
                Err(error) => last = error,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("GET {path} never succeeded: {last}");
    }

    /// Poll `/api/status` until `predicate` holds on the parsed payload.
    fn wait_for_status(
        &self,
        what: &str,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        let mut last = serde_json::Value::Null;
        while Instant::now() < deadline {
            if let Ok(body) = fetch(&self.url("/api/status"))
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
            {
                if predicate(&value) {
                    return value;
                }
                last = value;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("condition {what:?} never held; last status was {last}");
    }
}

impl Daemon {
    /// Stop the daemon but keep its data directory, so a later command can read what it wrote.
    ///
    /// Dropping the whole `Daemon` would delete the directory with it, which silently turns
    /// "read the database afterwards" into "read a database that no longer exists".
    fn stop(mut self) -> tempfile::TempDir {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Replace the field so `Drop` has nothing left to remove.
        std::mem::replace(
            &mut self.data_dir,
            tempfile::tempdir().expect("placeholder dir"),
        )
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Schema version this build migrates a database to.
fn schema_version() -> u32 {
    agentbench::watch::store::migrations::target_version()
}

/// Write records into a dashboard database before any daemon has opened it.
///
/// Goes through the real [`Store`] rather than raw SQL, so the rows are exactly the shape the daemon
/// produces — including the machine id, which has to match or the daemon will read none of them.
///
/// [`Store`]: agentbench::watch::store::Store
fn seed(data_dir: &Path, records: Vec<agentbench::watch::store::Record>) {
    use agentbench::watch::store::Store;
    let inventory = agentbench::system::inventory(false);
    let store = Store::open(&data_dir.join("watch.db"), &inventory).expect("open seeded database");
    let sink = store.sink();
    for record in records {
        assert!(sink.send(record), "the seed queue should not be full");
    }
    drop(sink);
    store.shutdown().expect("commit the seeded rows");
}

/// One probe run measuring small-file throughput, as the prober would have written it.
fn seeded_probe(ts: i64, ops: f64) -> agentbench::watch::store::Record {
    seeded_probe_at_clock(ts, ops, 136.0)
}

/// The same run, with the clock a caller chooses.
///
/// Split out so a test can seed the case the conditions line exists for: a judged series that dropped on a
/// day the part was running well below its usual clock.
fn seeded_probe_at_clock(ts: i64, ops: f64, clock: f32) -> agentbench::watch::store::Record {
    use agentbench::watch::store::{Covariates, MetricSource, ProbeMetric, ProbeRun};
    ProbeRun {
        ts,
        covariates: Covariates {
            cpu_percent: Some(3.0),
            scanner_percent: Some(0.1),
            agent_percent: Some(0.5),
            agent_active: false,
            contended: false,
            on_battery: Some(false),
            clock_percent: Some(clock),
            disk_write_bytes_s: Some(64_000.0),
            scratch_free_bytes: Some(110 << 30),
        },
        processes: Vec::new(),
        metrics: vec![ProbeMetric {
            name: "filesystem.small_file_ops_s".into(),
            value: ops,
            unit: "ops/s".into(),
            lower_is_better: false,
            source: MetricSource::Probe,
        }],
    }
    .into()
}

/// A short-lived command for `profile` to launch.
///
/// The test binary's own executable, so the test depends on nothing being installed and the child exits
/// immediately. `profile` needs a real process to time; what it does is irrelevant here, since these tests
/// are about the marker written around it.
fn profiled_command() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin("agentbench")
}

/// Ask the OS for an unused port by binding and immediately releasing one.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Minimal HTTP/1.0 GET, so the test suite needs no HTTP client dependency.
///
/// `host` is a parameter rather than derived from `authority` because the gap between the two is the
/// whole of the DNS-rebinding case: the connection lands on loopback either way, and only the header
/// says who the client thought it was calling.
fn raw_get(authority: &str, path: &str, host: &str) -> Result<(u16, String, String), String> {
    let mut stream = TcpStream::connect(authority).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    use std::io::Write;
    write!(
        stream,
        "GET /{path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("malformed response: {text:?}"))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), body.to_string()))
}

fn fetch(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("only http:// is supported")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (status, _, body) = raw_get(authority, path, authority)?;
    if status != 200 {
        return Err(format!("status {status}: {body}"));
    }
    Ok(body)
}

/// One `POST`, with every header the write gate inspects under the caller's control.
///
/// Hand-written for the same reason [`raw_get`] is: the interesting cases are the ones a well-behaved
/// client cannot produce. A cross-site request's `Sec-Fetch-Site`, its `Origin` and its `Content-Type` are
/// exactly what distinguishes a page of ours from a page of somebody else's, so a helper that set them
/// correctly on our behalf would be a helper that could only test the case that already works.
fn raw_post(
    authority: &str,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, String, String), String> {
    let mut stream = TcpStream::connect(authority).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| error.to_string())?;
    use std::io::Write;
    let mut request = format!(
        "POST /{path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("malformed response: {text:?}"))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), body.to_string()))
}

/// One `POST` whose body arrives chunked, so it carries no `Content-Length`.
///
/// Hand-written for the reason [`raw_post`] is: what makes this case interesting is a header a well-behaved
/// helper would supply. `fetch` with a string body always sends a length, so the dashboard's own pages cannot
/// produce this — `fetch` with a stream body, and most non-browser clients, send exactly this instead.
fn raw_post_chunked(
    authority: &str,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, String, String), String> {
    let mut stream = TcpStream::connect(authority).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| error.to_string())?;
    use std::io::Write;
    let mut request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\
         Transfer-Encoding: chunked\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    // One chunk, then the terminator. Enough to exercise the decoder without reimplementing it.
    request.push_str(&format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len()));
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("malformed response: {text:?}"))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_string(), body.to_string()))
}

/// The headers this dashboard's own pages send on a request that acts.
fn same_origin_headers(port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", "application/json".to_string()),
        ("Sec-Fetch-Site", "same-origin".to_string()),
        ("Origin", format!("http://127.0.0.1:{port}")),
    ]
}

/// Binding to loopback stops a network peer; it does not stop a browser.
///
/// A page on any origin can point a name it controls at 127.0.0.1 and then read every endpoint here
/// same-origin, which would expose real project paths and branch names. The request is indistinguishable
/// from a legitimate one at the socket, so this asserts on the only thing that differs: the `Host`.
#[test]
fn a_request_carrying_someone_elses_host_is_refused() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    let authority = format!("127.0.0.1:{}", daemon.port);

    let (status, head, _) = raw_get(&authority, "api/status", &authority).expect("own host");
    assert_eq!(status, 200, "{head}");
    let head = head.to_ascii_lowercase();
    assert!(head.contains("x-frame-options: deny"), "{head}");
    assert!(head.contains("frame-ancestors 'none'"), "{head}");

    for host in [
        "rebind.attacker.example".to_string(),
        format!("rebind.attacker.example:{}", daemon.port),
        format!("127.0.0.1.nip.io:{}", daemon.port),
    ] {
        let (status, head, _) = raw_get(&authority, "api/status", &host).expect("connect");
        assert_eq!(status, 421, "Host: {host} -> {head}");
    }
}

/// A correct `Host` is not enough for a request that starts work.
///
/// A body with no `Content-Length` is read, not discarded and then blamed on the document.
///
/// `tiny_http` reports no length for a chunked request and decodes the chunks through the same reader, so the
/// body is available under the same size cap — but the length was read as "zero" and the handler was handed an
/// empty `Vec`. A valid chunked `POST /api/compare` was therefore answered `unreadable report: EOF while
/// parsing value line 1 column 0`, which describes a body the server threw away.
///
/// `{}` is sent rather than two whole reports because the assertion is about which error comes back: a
/// complaint about a *missing field* proves the body arrived and parsed as JSON.
#[test]
fn a_chunked_request_body_reaches_the_handler() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    let authority = format!("127.0.0.1:{}", daemon.port);
    let headers = same_origin_headers(daemon.port);
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let (status, head, body) =
        raw_post_chunked(&authority, "api/compare", "{}", &pairs).expect("connect");
    assert_eq!(status, 400, "{head} {body}");
    assert!(
        body.contains("missing field"),
        "the body has to have reached serde rather than been discarded: {body}"
    );
    assert!(
        !body.contains("EOF while parsing"),
        "an EOF means the body was thrown away and the document blamed for it: {body}"
    );
}

/// This is the gate the benchmark endpoints exist behind, and the reason it is not the `Host` check above.
/// A form on `evil.example` submitting to `127.0.0.1:7878` sends exactly the `Host` this server expects —
/// what it cannot send is a same-origin `Sec-Fetch-Site`, our own `Origin`, and a JSON content type. Each of
/// the three is removed in turn here, because a gate that only fails when all three are wrong is a gate that
/// passes the interesting request.
#[test]
fn a_write_that_cannot_prove_it_came_from_this_dashboard_is_refused() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    let authority = format!("127.0.0.1:{}", daemon.port);
    let good = same_origin_headers(daemon.port);
    let good_pairs: Vec<(&str, &str)> = good
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    // The shape that is allowed through. `/api/compare` is used rather than `/api/bench` because it proves
    // the gate opened without starting a benchmark on the machine running the tests: an empty body reaches
    // the handler and is refused as an unreadable report, which is a 400 and not a 403.
    let (status, head, _) =
        raw_post(&authority, "api/compare", "{}", &good_pairs).expect("connect");
    assert_ne!(
        status, 403,
        "a same-origin write must not be refused: {head}"
    );

    // Each of the three conditions, broken on its own.
    let cases: Vec<(&str, Vec<(&str, String)>)> = vec![
        (
            "a cross-site fetch",
            vec![
                ("Content-Type", "application/json".to_string()),
                ("Sec-Fetch-Site", "cross-site".to_string()),
                ("Origin", format!("http://127.0.0.1:{}", daemon.port)),
            ],
        ),
        (
            "somebody else's origin",
            vec![
                ("Content-Type", "application/json".to_string()),
                ("Origin", "http://evil.example".to_string()),
            ],
        ),
        (
            "a form post, which needs no preflight",
            vec![
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("Sec-Fetch-Site", "same-origin".to_string()),
            ],
        ),
        ("no content type at all", vec![]),
    ];
    for (what, headers) in cases {
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let (status, head, _) = raw_post(&authority, "api/compare", "{}", &pairs).expect("connect");
        assert_eq!(status, 403, "{what} should be refused: {head}");
    }

    // And the same for the endpoint that would actually start a benchmark.
    let (status, head, _) = raw_post(
        &authority,
        "api/bench",
        "{\"preset\":\"quick\"}",
        &[("Content-Type", "text/plain")],
    )
    .expect("connect");
    assert_eq!(status, 403, "{head}");
}

/// A benchmark starts as a real child process, announces phases, and stops when told to.
///
/// Deliberately stops the run rather than letting it finish. A `quick` preset is forty-five seconds of real
/// load, which is the whole of this suite's timeout, and what is being tested here is the wiring — that the
/// child starts, that its `[n/8]` lines are parsed back into phases, that the state machine moves, and that
/// cancelling reaches the process. A full run is [`a_quick_benchmark_started_from_the_dashboard_writes_a_report`],
/// which is ignored by default.
#[test]
fn a_benchmark_started_from_the_dashboard_reports_its_phases_and_can_be_stopped() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    let authority = format!("127.0.0.1:{}", daemon.port);
    let headers = same_origin_headers(daemon.port);
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    // The options endpoint describes the form, including every preset's published limits.
    let options: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/bench/options")).expect("json");
    assert_eq!(options["allowed"], true, "{options}");
    assert_eq!(options["phase_count"], 8, "{options}");
    let quick = &options["presets"][0];
    assert_eq!(quick["name"], "quick");
    assert_eq!(quick["duration_limit_seconds"], 45);

    let body = format!(
        "{{\"preset\":\"quick\",\"offline\":true,\"live_llm\":false,\"target_dir\":{}}}",
        serde_json::to_string(&daemon.data_dir().display().to_string()).unwrap()
    );
    let (status, head, started) =
        raw_post(&authority, "api/bench", &body, &pairs).expect("connect");
    assert_eq!(status, 202, "{head} {started}");
    let started: serde_json::Value = serde_json::from_str(&started).expect("json");
    let run_id = started["run_id"].as_str().expect("a run id").to_string();

    // A second request is refused while the first is in flight, because two benchmarks measure each other.
    let (status, _, busy) = raw_post(&authority, "api/bench", &body, &pairs).expect("connect");
    assert_eq!(status, 409, "{busy}");
    assert!(busy.contains("still running"), "{busy}");

    // Poll until the child announces a phase, which proves the pipe is being read and `Phase::parse` agrees
    // with what `bench` printed.
    let deadline = Instant::now() + TIMEOUT;
    let mut last = String::new();
    let phase = loop {
        assert!(
            Instant::now() < deadline,
            "no phase was ever reported: {last}"
        );
        last = daemon.get("/api/bench/run");
        let state: serde_json::Value = serde_json::from_str(&last).expect("json");
        assert_eq!(state["state"], "running", "the run ended too early: {last}");
        assert_eq!(state["run_id"], run_id.as_str());
        if let Some(number) = state["phase"]["number"].as_i64() {
            break (
                number,
                state["phase"]["label"].as_str().unwrap_or("").to_string(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(phase.0 >= 1 && phase.0 <= 8, "{phase:?}");
    assert!(!phase.1.is_empty(), "a phase carries a label: {phase:?}");

    // Stopping it reaches the process, and the run is reported as having produced nothing.
    let (status, _, cancelled) =
        raw_post(&authority, "api/bench/cancel", "{}", &pairs).expect("connect");
    assert_eq!(status, 200, "{cancelled}");

    let deadline = Instant::now() + TIMEOUT;
    let finished = loop {
        assert!(Instant::now() < deadline, "the run never finished: {last}");
        last = daemon.get("/api/bench/run");
        let state: serde_json::Value = serde_json::from_str(&last).expect("json");
        if state["state"] == "finished" {
            break state;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(finished["cancelled"], true, "{finished}");
    assert_eq!(
        finished["ok"], false,
        "a stopped run wrote no report: {finished}"
    );
    assert!(finished["report_path"].is_null(), "{finished}");
    // The request is echoed back, so the page can say what was stopped.
    assert_eq!(finished["request"]["preset"], "quick");
    assert_eq!(finished["request"]["live_llm"], false);

    // And the slot is free again.
    let (status, _, restarted) = raw_post(&authority, "api/bench", &body, &pairs).expect("connect");
    assert_eq!(status, 202, "{restarted}");
    let (status, _, _) = raw_post(&authority, "api/bench/cancel", "{}", &pairs).expect("connect");
    assert_eq!(status, 200);
}

/// A full `quick` run, end to end, producing a report the compare endpoint accepts.
///
/// Ignored by default and not because it is unreliable: it is forty-five seconds of deliberate CPU, disk and
/// memory load, which on a shared runner would slow every other test in this suite for the benefit of one.
/// Run it with `cargo test --locked --all-targets -- --ignored` when the run supervisor changes.
#[test]
#[ignore = "runs a real 45-second benchmark; run explicitly with --ignored"]
fn a_quick_benchmark_started_from_the_dashboard_writes_a_report() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    let authority = format!("127.0.0.1:{}", daemon.port);
    let headers = same_origin_headers(daemon.port);
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let body = format!(
        "{{\"preset\":\"quick\",\"offline\":true,\"live_llm\":false,\"target_dir\":{}}}",
        serde_json::to_string(&daemon.data_dir().display().to_string()).unwrap()
    );
    let (status, head, _) = raw_post(&authority, "api/bench", &body, &pairs).expect("connect");
    assert_eq!(status, 202, "{head}");

    // Its own bound: a quick preset's own duration limit is this suite's whole TIMEOUT.
    let deadline = Instant::now() + Duration::from_secs(240);
    let finished = loop {
        assert!(Instant::now() < deadline, "the benchmark never finished");
        let state: serde_json::Value =
            serde_json::from_str(&daemon.get("/api/bench/run")).expect("json");
        if state["state"] == "finished" {
            break state;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(finished["ok"], true, "{finished}");
    assert_eq!(finished["cancelled"], false, "{finished}");
    assert_eq!(finished["exit_code"], 0, "{finished}");

    // The marker lands in *this* daemon's database, which is the whole point of the daemon telling the child
    // which one to write to. Without that, a daemon on `--data-dir` recorded nothing for a run it started
    // itself — leaving the cliff the run left in its own passive series unannotated for a later baseline to
    // average in — and put the marker and the run's metrics in whichever database the per-user default
    // resolved to instead.
    let status: serde_json::Value = serde_json::from_str(&daemon.get("/api/status")).expect("json");
    assert_eq!(
        status["health"]["run_markers"].as_i64(),
        Some(1),
        "the run this daemon started has to be marked in the database it is writing: {status}"
    );
    // And the run's own measurements are readable from here as a `bench:` series, under the source that keeps
    // them out of the probe series they share a table with.
    let series: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/series?metric=bench:cpu.single_mops_s"))
            .expect("json");
    assert!(
        !series["points"].as_array().expect("points").is_empty(),
        "the benchmark's metrics belong in the same database as its marker: {series}"
    );

    // The report exists, parses, and is the schema this binary compares.
    let path = finished["report_path"].as_str().expect("a report path");
    let report = agentbench::report::read_report(Path::new(path)).expect("a readable report");
    assert_eq!(report.schema_version, schema_version());
    assert_eq!(report.config.preset.as_deref(), Some("quick"));
    assert!(!report.metrics.is_empty(), "a benchmark produces metrics");
    // Written where the daemon said it would be.
    assert!(
        Path::new(path).starts_with(daemon.data_dir().join("reports")),
        "{path}"
    );
    assert!(finished["markdown_path"].as_str().is_some(), "{finished}");

    // And it compares with itself, through the endpoint the page uses.
    let document = std::fs::read_to_string(path).expect("read the report");
    let payload = format!("{{\"baseline\":{document},\"candidate\":{document}}}");
    let (status, head, comparison) =
        raw_post(&authority, "api/compare", &payload, &pairs).expect("connect");
    assert_eq!(status, 200, "{head} {comparison}");
    let comparison: serde_json::Value = serde_json::from_str(&comparison).expect("json");
    assert!(
        !comparison["metrics"]
            .as_array()
            .expect("metrics")
            .is_empty(),
        "{comparison}"
    );
    // A report compared with itself has changed in nothing.
    for delta in comparison["metrics"].as_array().unwrap() {
        assert_eq!(delta["change_percent"], 0.0, "{delta}");
    }
    assert!(
        comparison["environment"]
            .as_array()
            .expect("environment")
            .is_empty(),
        "{comparison}"
    );
}

#[test]
fn the_daemon_collects_serves_and_shuts_down_cleanly() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();

    // The config file is written on first run so it is self-documenting.
    assert!(
        daemon.data_dir().join("watch.toml").is_file(),
        "watch.toml should be created on first run"
    );

    // Collection actually reaches the database, not just the channel.
    let status = daemon.wait_for_status("a sample is recorded", |status| {
        status["health"]["samples"].as_i64().unwrap_or(0) > 0
    });
    assert_eq!(status["collecting"], true, "{status}");
    // Against the version this build migrates to, not a literal: what matters is that the daemon reports
    // the schema it actually applied, and a literal here goes stale on every migration.
    assert_eq!(
        status["health"]["schema_version"].as_i64(),
        Some(i64::from(schema_version())),
        "{status}"
    );
    assert!(
        status["series"]
            .as_array()
            .is_some_and(|series| series.iter().any(|s| s == "cpu_percent")),
        "status should advertise available series: {status}"
    );

    // The live payload carries a real observation.
    let live: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/live")).expect("live json");
    assert!(live["machine_id"].as_str().is_some_and(|id| !id.is_empty()));
    let sample = &live["sample"];
    assert!(sample["total_memory"].as_i64().unwrap_or(0) > 0, "{live}");
    assert!(sample["process_count"].as_i64().unwrap_or(0) > 0, "{live}");

    // The chart endpoint returns usable points with a gap threshold.
    let series: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/series?metric=cpu_percent")).expect("series json");
    assert_eq!(series["metric"], "cpu_percent");
    assert!(series["gap_ms"].as_i64().unwrap_or(0) > 0, "{series}");
    assert!(
        !series["points"]
            .as_array()
            .expect("points array")
            .is_empty(),
        "{series}"
    );

    // The page and its embedded assets are served, so the dashboard needs no network.
    let index = daemon.get("/");
    assert!(index.contains("AgentBench"), "index should render");
    assert!(
        index.contains("/assets/uplot.min.js"),
        "index should load uPlot"
    );
    assert!(
        daemon.get("/assets/uplot.min.js").len() > 10_000,
        "vendored uPlot should be served"
    );
    for asset in [
        "/assets/app.js",
        "/assets/chart.js",
        "/assets/format.js",
        "/assets/series.js",
    ] {
        assert!(!daemon.get(asset).is_empty(), "{asset} should be served");
    }
}

/// The probe stream end to end: a controlled workload becomes a chart, a tile, and a covariate.
///
/// This is the one test that lets the daemon actually load the machine, because nothing cheaper proves the
/// part that matters — that the workloads run on real files, the covariates are read either side of them,
/// the metrics reach the database under names the catalogue knows, and the scratch directory is left clean
/// for the next run.
#[test]
fn probes_run_are_charted_and_carry_their_covariates() {
    let daemon = Daemon::start_probing();
    daemon.wait_until_listening();

    let status = daemon.wait_for_status("a probe is recorded", |status| {
        status["health"]["probe_runs"].as_i64().unwrap_or(0) > 0
    });
    assert!(
        status["series"].as_array().is_some_and(|series| series
            .iter()
            .any(|name| name == "probe:filesystem.small_file_ops_s")),
        "the probe family should be advertised: {status}"
    );

    // The live tile's payload: a real measurement with real covariates.
    let live: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/live")).expect("live json");
    let probe = &live["probe"];
    assert!(probe["ts"].as_i64().is_some(), "{live}");
    assert!(
        probe["cpu_at"].as_f64().is_some(),
        "a probe must say what it was competing with: {live}"
    );
    assert!(
        probe["metrics"].as_i64().unwrap_or(0) >= 4,
        "a probe should record several measurements: {live}"
    );
    // A contended run has to name the threshold it crossed rather than leaving the page to infer one from
    // three covariates, which is how a run tagged purely by the disk rule came to report "the machine was
    // busy". Whether *this* run is contended depends on the machine running the suite, so the assertion is
    // the implication rather than the tag: a cause exactly when there is contention to explain.
    assert_eq!(
        probe["contended"].as_bool().expect("a tag either way"),
        probe["cause"].is_string(),
        "a cause belongs to a contended run and to no other: {live}"
    );

    // The conditions family: the covariates of those same runs, reachable as charts.
    for metric in [
        "cond:cpu_at",
        "cond:clock_percent",
        "cond:scratch_free_bytes",
    ] {
        let series: serde_json::Value =
            serde_json::from_str(&daemon.get(&format!("/api/series?metric={metric}")))
                .expect("conditions json");
        assert!(
            series["unit"].as_str().is_some_and(|unit| !unit.is_empty()),
            "{metric} must report a unit for the axis to derive from: {series}"
        );
        assert!(
            series["lower_is_better"].is_null(),
            "a covariate has no direction: {series}"
        );
    }
    // `cpu_at` and `scratch_free_bytes` are the two covariates every platform answers, so they are the two
    // whose points can be asserted rather than merely their shape.
    for metric in ["cond:cpu_at", "cond:scratch_free_bytes"] {
        let series: serde_json::Value =
            serde_json::from_str(&daemon.get(&format!("/api/series?metric={metric}")))
                .expect("conditions json");
        assert!(
            series["points"]
                .as_array()
                .is_some_and(|points| !points.is_empty()),
            "{metric} should have a point per run: {series}"
        );
    }
    // Every advertised conditions series is one the endpoint answers, so the list in a 400 is usable.
    let advertised: Vec<String> = status["series"]
        .as_array()
        .expect("series list")
        .iter()
        .filter_map(|name| name.as_str())
        .filter(|name| name.starts_with("cond:"))
        .map(str::to_string)
        .collect();
    assert_eq!(advertised.len(), 6, "six covariates: {advertised:?}");
    for metric in advertised {
        let series: serde_json::Value =
            serde_json::from_str(&daemon.get(&format!("/api/series?metric={metric}")))
                .expect("conditions json");
        assert_eq!(series["metric"], metric, "{series}");
    }

    // A metric the probe genuinely runs, under the name the benchmark uses for the same workload.
    let series: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/series?metric=probe:filesystem.small_file_ops_s"))
            .expect("series json");
    assert_eq!(series["unit"], "ops/s", "{series}");
    assert_eq!(series["lower_is_better"], false, "{series}");
    let points = series["points"].as_array().expect("points array");
    assert!(!points.is_empty(), "{series}");
    assert!(
        points
            .iter()
            .all(|point| point["value"].as_f64().is_some_and(|value| value > 0.0)),
        "every probe should have measured something: {series}"
    );

    // A metric the probe deliberately does not run: an 8 MiB read is page cache, not disk.
    let cached: serde_json::Value = serde_json::from_str(
        &daemon.get("/api/series?metric=probe:filesystem.sequential_read_mib_s"),
    )
    .expect("series json");
    assert!(
        cached["points"].as_array().expect("points").is_empty(),
        "the cached read must not be recorded: {cached}"
    );

    // Probes write beside the database by default, which is the point: the volume measured is the one the
    // user chose for the data directory rather than whatever `%TEMP%` happens to be on.
    //
    // What is deliberately not asserted here is that the directory is empty. This daemon is killed at an
    // arbitrary instant and probes run back to back at the interval the test asked for, so a kill landing
    // mid-workload legitimately leaves files behind. That the *prober* cleans up after itself is a unit
    // test, where the boundary is a function return rather than a signal.
    let data_dir = daemon.stop();
    assert!(
        data_dir.path().join("probe-scratch").is_dir(),
        "probes should write inside the data directory"
    );
}

/// The session stream end to end: a transcript on disk becomes a chart and a tile.
#[test]
fn transcripts_are_imported_charted_and_summarised() {
    let transcripts = tempfile::tempdir().expect("temp transcripts dir");
    let project = transcripts.path().join("D--Stuff-Example");
    std::fs::create_dir_all(&project).expect("create project dir");

    // Timestamps have to be recent: the range the dashboard asks for ends now, and the tiles count
    // activity since local midnight.
    let base = chrono::Utc::now() - chrono::Duration::seconds(5);
    let at = |offset_ms: i64| {
        (base + chrono::Duration::milliseconds(offset_ms))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let transcript = format!(
        r#"{{"type":"user","uuid":"p1","timestamp":"{prompt}","message":{{"content":"go"}}}}
{{"type":"assistant","uuid":"a1","parentUuid":"p1","requestId":"req_1","timestamp":"{answer}","sessionId":"s1","cwd":"D:\\Stuff\\Example","gitBranch":"main","version":"2.1.187","effort":"high","message":{{"model":"claude-opus-5","usage":{{"input_tokens":100,"output_tokens":42,"cache_read_input_tokens":900,"cache_creation_input_tokens":0,"service_tier":"standard"}},"content":[{{"type":"tool_use","id":"t1","name":"Read"}}]}}}}
{{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1","timestamp":"{result}","cwd":"D:\\Stuff\\Example","toolUseResult":{{"file":"x"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t1"}}]}}}}
"#,
        prompt = at(0),
        answer = at(1_500),
        result = at(1_511),
    );
    std::fs::write(project.join("session.jsonl"), &transcript).expect("write transcript");

    let daemon = Daemon::start_with_transcripts(transcripts.path());
    daemon.wait_until_listening();

    let status = daemon.wait_for_status("the transcript is imported", |status| {
        status["health"]["session_turns"].as_i64().unwrap_or(0) > 0
    });
    assert_eq!(
        status["health"]["session_tools"].as_i64(),
        Some(1),
        "{status}"
    );
    assert_eq!(
        status["health"]["imported_files"].as_i64(),
        Some(1),
        "{status}"
    );
    assert_eq!(
        status["health"]["import_errors"].as_i64(),
        Some(0),
        "{status}"
    );
    assert!(
        status["series"]
            .as_array()
            .is_some_and(|series| series.iter().any(|name| name == "tool_read_ms")),
        "derived series should be advertised: {status}"
    );

    // The derived series is bucketed and charts the latency between the two rows.
    let series: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/series?metric=tool_read_ms")).expect("series json");
    assert!(series["bucket_ms"].as_i64().unwrap_or(0) > 0, "{series}");
    let points = series["points"].as_array().expect("points array");
    assert_eq!(points.len(), 1, "{series}");
    assert_eq!(points[0]["value"].as_f64(), Some(11.0), "{series}");

    // And the same numbers appear in the tiles, counted since local midnight.
    let payload: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/today")).expect("today json");
    let today = &payload["today"];
    let day_start = payload["day_start_ts"].as_i64().expect("day start");
    if base.timestamp_millis() >= day_start {
        assert_eq!(today["turns"].as_i64(), Some(1), "{payload}");
        assert_eq!(today["tool_calls"].as_i64(), Some(1), "{payload}");
        assert_eq!(today["sessions"].as_i64(), Some(1), "{payload}");
        assert_eq!(today["output_tokens"].as_i64(), Some(42), "{payload}");
        assert_eq!(today["tool_read_p50_ms"].as_f64(), Some(11.0), "{payload}");
        // 900 of 1000 prompt tokens came from the cache.
        assert_eq!(today["cache_hit_ratio"].as_f64(), Some(0.9), "{payload}");
    }

    // An unchanged transcript is not read again, which is what the watermark is for.
    let status = daemon.wait_for_status("the importer settles", |status| {
        status["health"]["session_turns"].as_i64() == Some(1)
    });
    assert_eq!(
        status["health"]["session_turns"].as_i64(),
        Some(1),
        "{status}"
    );
}

/// A foreground run annotates the dashboard, and does so without a daemon needing to be up.
///
/// Run through `profile` rather than `bench` on purpose: a benchmark takes minutes and saturates the
/// runner, whereas this exercises the identical marker path — `mark_run` before, `finish` after — in about
/// a second. `--status` is then the assertion, so the test reads the same payload a user would.
#[test]
fn a_foreground_run_marks_itself_in_an_existing_dashboard_database() {
    // A database has to exist first, and only the daemon creates one: a foreground run must never bring
    // a metrics database into being as a side effect.
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    daemon.wait_for_status("the database is written", |status| {
        status["health"]["samples"].as_i64().unwrap_or(0) > 0
    });
    let data_dir = daemon.stop();

    let reports = tempfile::tempdir().expect("temp reports dir");
    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        // The marker resolves the per-user data directory, and this is the override that redirects it.
        .env("AGENTBENCH_DATA_DIR", data_dir.path())
        .args(["profile", "--label", "marker-test", "--output"])
        .arg(reports.path().join("profile.json"))
        .arg("--")
        .arg(profiled_command())
        .arg("--version")
        .output()
        .expect("run profile");
    assert!(
        output.status.success(),
        "profile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .args(["dashboard", "--status", "--data-dir"])
        .arg(data_dir.path())
        .output()
        .expect("run status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("Marked runs:    1"),
        "the run should be marked once, not twice: {stdout}"
    );
}

/// The common case: no dashboard database, so a foreground run must not create one.
#[test]
fn a_foreground_run_does_not_create_a_dashboard_database() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let reports = tempfile::tempdir().expect("temp reports dir");
    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .env("AGENTBENCH_DATA_DIR", data_dir.path())
        .args(["profile", "--label", "no-dashboard", "--output"])
        .arg(reports.path().join("profile.json"))
        .arg("--")
        .arg(profiled_command())
        .arg("--version")
        .output()
        .expect("run profile");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !data_dir.path().join("watch.db").exists(),
        "collecting is something the user starts, not something a profile run decides for them"
    );
}

/// A verdict, end to end: seeded days go through the real writer, the real local-day bucketing and the
/// real endpoint.
///
/// This is the test the phase before this one did not have. Every unit test of the classification rule
/// passed while the numbers fed to it were wrong, so the rule is checked here against data that travelled
/// the whole way rather than against arguments a test chose.
#[test]
fn verdicts_compare_today_against_a_seeded_baseline() {
    use agentbench::watch::analysis::day;

    let data_dir = tempfile::tempdir().expect("temp data dir");
    let today = day::today();
    let mut records = Vec::new();
    // Five settled days at 4,000 ops/s. Four runs each: enough for a day to have a median at all.
    for previous in day::preceding(today, 5) {
        for offset in 0..4 {
            records.push(seeded_probe(
                previous.start_ms + offset * 3_600_000,
                4_000.0,
            ));
        }
    }
    // Today, at half that, and with the part running at 96% of nominal instead of its usual 136. Offsets in
    // seconds so the run stamps stay in the past whatever time it is.
    for offset in 0..4 {
        records.push(seeded_probe_at_clock(
            today.start_ms + offset * 1_000,
            2_000.0,
            96.0,
        ));
    }
    seed(data_dir.path(), records);

    let daemon = Daemon::start_in(data_dir, &[]);
    daemon.wait_until_listening();
    let payload: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/verdicts")).expect("valid json");

    let small_files = payload["comparisons"]
        .as_array()
        .expect("comparisons")
        .iter()
        .find(|one| one["metric"] == "probe:filesystem.small_file_ops_s")
        .expect("the curated set includes small-file operations");

    assert_eq!(small_files["verdict"], "worse", "{small_files}");
    assert_eq!(small_files["today"].as_f64(), Some(2_000.0));
    assert_eq!(small_files["baseline"]["median"].as_f64(), Some(4_000.0));
    assert_eq!(small_files["baseline"]["days"].as_i64(), Some(5));
    assert_eq!(
        small_files["baseline"]["observations"].as_i64(),
        Some(20),
        "the count behind the band is part of the finding"
    );
    assert_eq!(small_files["today_observations"].as_i64(), Some(4));
    let delta = small_files["delta_percent"].as_f64().expect("a delta");
    assert!((delta + 50.0).abs() < 1e-6, "{delta}");
    assert_eq!(payload["window_days"], 7);
    assert_eq!(payload["day_start_ms"].as_i64(), Some(today.start_ms));

    // Five identical days have no measurable spread, and the band says so rather than pretending to.
    assert_eq!(small_files["baseline"]["width_is_floor"], true);
    assert!(
        small_files["note"]
            .as_str()
            .is_some_and(|note| note.contains("minimum width")),
        "{small_files}"
    );

    // The other half of the question: not only that today is worse, but what was different about it. The
    // clock moved from 136% of nominal to 96%, well outside the band its own five days produce, so it earns
    // a clause; the disk rate and the free space did not move at all and must not.
    let conditions = &small_files["conditions"];
    assert!(
        conditions["summary"]
            .as_str()
            .is_some_and(|line| line.contains("clock") && line.contains("96%")),
        "the throttled clock is the explanation this feature exists to give: {small_files}"
    );
    let changes = conditions["changes"].as_array().expect("changes array");
    assert_eq!(
        changes.len(),
        1,
        "only the covariate that moved: {conditions}"
    );
    assert_eq!(changes[0]["metric"], "cond:clock_percent");
    assert_eq!(changes[0]["today"].as_f64(), Some(96.0));
    assert_eq!(changes[0]["baseline"].as_f64(), Some(136.0));
    // Every clause names a series the reader can go and open, and the endpoint answers it. The range is
    // given explicitly rather than left to the endpoint's 48-hour default, which would reach two of the six
    // seeded days: the reader following this clause is asking about the window the verdict used.
    let charted: serde_json::Value = serde_json::from_str(&daemon.get(&format!(
        "/api/series?metric=cond:clock_percent&contended=exclude&from={}&to={}",
        today.start_ms - 7 * 86_400_000,
        today.start_ms + 86_400_000,
    )))
    .expect("conditions json");
    assert!(
        charted["points"]
            .as_array()
            .is_some_and(|points| points.len() == 24),
        "five seeded days and today, four runs each: {charted}"
    );

    // A series with no seeded history reaches no verdict rather than reporting that all is well — and a tile
    // with no verdict carries no explanation either, because there is nothing yet to explain.
    let cpu = payload["comparisons"]
        .as_array()
        .expect("comparisons")
        .iter()
        .find(|one| one["metric"] == "probe:cpu.single_mops_s")
        .expect("single-core CPU is judged too");
    assert_eq!(cpu["verdict"], "insufficient", "{cpu}");
    assert!(cpu["conditions"].is_null(), "{cpu}");

    // The same numbers reach the command line, from the same analysis.
    let temp = daemon.stop();
    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .args(["dashboard", "--status", "--data-dir"])
        .arg(temp.path())
        .output()
        .expect("run status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Today vs baseline"), "{stdout}");
    assert!(stdout.contains("small-file operations"), "{stdout}");
    assert!(stdout.contains("worse"), "{stdout}");
    assert!(stdout.contains("(-50.0%)"), "{stdout}");
    // Including the conditions line. Two display faults have been caught by reading `--status` rather than
    // by any test, so this surface stays as complete as the page's.
    assert!(
        stdout.contains("clean probes: clock 96% today against 136%"),
        "{stdout}"
    );
}

/// Retention, end to end: the worker is spawned, its instruction reaches the writer, and the chart survives.
///
/// Seeded rather than waited for. Samples have to be older than the retention window and older than the
/// minute in progress, and no amount of waiting produces that within a test's patience.
#[test]
fn retention_summarises_old_samples_and_the_series_survives_it() {
    use agentbench::watch::store::Sample;

    let data_dir = tempfile::tempdir().expect("temp data dir");
    // Two days old, so any retention window of a day or more covers them. Aligned to a minute boundary so
    // that twenty-four samples at a five-second cadence fall into exactly two buckets rather than
    // straddling three, which would make the assertion below depend on the time the test happened to run.
    let old =
        (agentbench::watch::store::now_ms() - 2 * 24 * 60 * 60 * 1000).div_euclid(60_000) * 60_000;
    let records: Vec<agentbench::watch::store::Record> = (0..24)
        .map(|index| {
            Sample {
                // Five-second cadence across two minutes: twelve samples per bucket.
                ts: old + index * 5_000,
                cpu_percent: 10.0 + index as f32,
                used_memory: 1 << 30,
                total_memory: 16 << 30,
                used_swap: 0,
                process_count: 400,
                scanner_cpu: None,
                agent_cpu: None,
                agent_rss: None,
                agent_processes: None,
                agent_write_bytes_s: None,
                scanner_write_bytes_s: None,
            }
            .into()
        })
        .collect();
    seed(data_dir.path(), records);
    std::fs::write(
        data_dir.path().join("watch.toml"),
        "[retention]\nsamples_raw_days = 1\n",
    )
    .expect("write the retention window");

    let daemon = Daemon::start_in(data_dir, &[]);
    daemon.wait_until_listening();
    // The pass reports itself through the operational log, which is how a daemon behind a scheduler
    // explains what it did to data nobody watched it touch.
    let status = daemon.wait_for_status("a retention pass runs", |status| {
        status["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|event| event["source"] == "retention"))
    });
    let report = status["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| event["source"] == "retention")
        .and_then(|event| event["message"].as_str())
        .expect("a retention message")
        .to_string();
    assert!(report.contains("rolled up 2 minute(s)"), "{report}");
    assert!(report.contains("pruned 24 raw sample(s)"), "{report}");

    // The point of the exercise: the two-day-old stretch is still chartable, now as summarised minutes.
    let from = old - 60_000;
    let to = old + 5 * 60_000;
    let series: serde_json::Value = serde_json::from_str(&daemon.get(&format!(
        "/api/series?metric=cpu_percent&from={from}&to={to}"
    )))
    .expect("valid json");
    assert_eq!(series["resolution"], "rollup", "{series}");
    assert_eq!(series["rollup_reducer"], "mean");
    assert_eq!(series["bucket_ms"], 60_000);
    let points = series["points"].as_array().expect("points");
    assert_eq!(points.len(), 2, "two summarised minutes: {series}");
    assert!(
        points
            .iter()
            .all(|point| point["value"].as_f64().is_some_and(|value| value > 0.0)),
        "{series}"
    );

    // A range reaching back past the boundary is the mixed case: summarised minutes at the old end,
    // untouched samples at the new one, in one ordered series with nothing repeated across the join.
    //
    // Asked for explicitly rather than left to the default window. The default is two days and the
    // seeded stretch is two days old, so whether its summarised minutes fall inside it depends on how
    // long the daemon took to start — which made this assertion fail about one run in three.
    let spanning: serde_json::Value = serde_json::from_str(&daemon.get(&format!(
        "/api/series?metric=cpu_percent&from={}&to={}",
        old - 60_000,
        agentbench::watch::store::now_ms()
    )))
    .expect("valid json");
    assert_eq!(spanning["resolution"], "mixed", "{spanning}");
    let stamps: Vec<i64> = spanning["points"]
        .as_array()
        .expect("points")
        .iter()
        .filter_map(|point| point["ts"].as_i64())
        .collect();
    assert!(
        stamps.windows(2).all(|pair| pair[0] < pair[1]),
        "the join must not reorder or repeat an instant: {stamps:?}"
    );
    assert!(
        stamps.first().is_some_and(|first| *first <= old + 60_000),
        "the summarised end must be present: {stamps:?}"
    );
}

#[test]
fn a_second_daemon_on_the_same_data_dir_refuses_to_start() {
    let first = Daemon::start(&[]);
    first.wait_until_listening();

    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .arg("dashboard")
        .arg("--data-dir")
        .arg(first.data_dir())
        .arg("--port")
        .arg(free_port().to_string())
        .output()
        .expect("run second daemon");

    assert!(!output.status.success(), "the second daemon must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already running"),
        "expected a single-instance error, got: {stderr}"
    );
}

#[test]
fn status_reports_before_any_daemon_has_run() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .arg("dashboard")
        .arg("--status")
        .arg("--data-dir")
        .arg(data_dir.path())
        .output()
        .expect("run status");

    // No database yet is a clear message, not a panic or a silent success.
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no watch database"),
        "expected a helpful message, got: {stderr}"
    );
}

#[test]
fn status_reads_a_database_written_by_a_previous_run() {
    let daemon = Daemon::start(&[]);
    daemon.wait_until_listening();
    daemon.wait_for_status("a sample is recorded", |status| {
        status["health"]["samples"].as_i64().unwrap_or(0) > 0
    });
    // Stop the daemon so its instance lock is released, keeping the directory it wrote to.
    let data_dir = daemon.stop();

    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .arg("dashboard")
        .arg("--status")
        .arg("--data-dir")
        .arg(data_dir.path())
        .output()
        .expect("run status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "status failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("Schema version: {}", schema_version())),
        "{stdout}"
    );
    assert!(stdout.contains("samples"), "{stdout}");
    assert!(stdout.contains("Import errors:  0"), "{stdout}");
}

#[test]
fn collection_works_with_the_server_disabled() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let mut child = Command::cargo_bin("agentbench")
        .expect("built binary")
        .arg("dashboard")
        .arg("--no-serve")
        .arg("--data-dir")
        .arg(data_dir.path())
        .arg("--sample-interval")
        .arg("200ms")
        .arg("--no-sessions")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn collector");

    let status = || {
        Command::cargo_bin("agentbench")
            .expect("built binary")
            .arg("dashboard")
            .arg("--status")
            .arg("--data-dir")
            .arg(data_dir.path())
            .output()
            .expect("run status")
    };

    // Wait for the collector to have written a schema, not merely for the file to appear. Opening a
    // connection creates the file before the first migration commits, and a status read is
    // deliberately not allowed to migrate it into existence — that is how a newer binary asked for a
    // status line used to upgrade the schema underneath an older daemon still collecting on it.
    let deadline = Instant::now() + TIMEOUT;
    let database = data_dir.path().join("watch.db");
    let mut collecting = false;
    while Instant::now() < deadline {
        if database.exists() && status().status.success() {
            collecting = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        collecting,
        "the database should be created and readable without a server"
    );

    // No port was opened, and the data is still readable after the collector is gone.
    let output = status();
    assert!(
        output.status.success(),
        "status after --no-serve failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_tui_moved_to_top_and_dashboard_took_over_the_name() {
    let help = |args: &[&str]| -> String {
        let output = Command::cargo_bin("agentbench")
            .expect("built binary")
            .args(args)
            .output()
            .expect("run --help");
        assert!(output.status.success(), "{args:?} --help failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // The TUI still exists, under its new name, with its flags intact.
    let top = help(&["top", "--help"]);
    for flag in ["--pid", "--name", "--interval-ms"] {
        assert!(top.contains(flag), "top should keep {flag}: {top}");
    }

    // `dashboard` is now the daemon: its own flags are advertised and the TUI's are hidden.
    let dashboard = help(&["dashboard", "--help"]);
    for flag in ["--port", "--data-dir", "--no-serve", "--status"] {
        assert!(
            dashboard.contains(flag),
            "dashboard should offer {flag}: {dashboard}"
        );
    }
    assert!(
        !dashboard.contains("--interval-ms"),
        "the deprecated TUI flags must stay hidden: {dashboard}"
    );

    // Both commands appear in the top-level help, so the rename is discoverable.
    let root = help(&["--help"]);
    assert!(root.contains("top"), "{root}");
    assert!(root.contains("dashboard"), "{root}");
}

/// The deprecation shim itself is deliberately not exercised end-to-end: it hands control to the
/// interactive TUI, and `enable_raw_mode` succeeds even with piped stdout, so the child would run
/// until killed rather than exiting on its own.
#[test]
fn the_deprecated_tui_flags_are_still_accepted_by_the_parser() {
    // `--help` alongside the deprecated flag proves clap still accepts it without running anything.
    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .args(["dashboard", "--pid", "1", "--help"])
        .output()
        .expect("run deprecated form with --help");
    assert!(
        output.status.success(),
        "the deprecated flag should still parse: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
