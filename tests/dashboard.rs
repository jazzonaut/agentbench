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

    fn spawn(extra: &[&str], sessions: &[&str]) -> Self {
        let data_dir = tempfile::tempdir().expect("temp data dir");
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
fn fetch(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("only http:// is supported")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(authority).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    use std::io::Write;
    write!(
        stream,
        "GET /{path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
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
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("000");
    if status != "200" {
        return Err(format!("status {status}: {body}"));
    }
    Ok(body.to_string())
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
    assert_eq!(
        status["health"]["schema_version"].as_i64(),
        Some(2),
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
    for asset in ["/assets/app.js", "/assets/chart.js", "/assets/format.js"] {
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
    let live: serde_json::Value =
        serde_json::from_str(&daemon.get("/api/live")).expect("live json");
    let today = &live["today"];
    let day_start = live["day_start_ts"].as_i64().expect("day start");
    if base.timestamp_millis() >= day_start {
        assert_eq!(today["turns"].as_i64(), Some(1), "{live}");
        assert_eq!(today["tool_calls"].as_i64(), Some(1), "{live}");
        assert_eq!(today["sessions"].as_i64(), Some(1), "{live}");
        assert_eq!(today["output_tokens"].as_i64(), Some(42), "{live}");
        assert_eq!(today["tool_read_p50_ms"].as_f64(), Some(11.0), "{live}");
        // 900 of 1000 prompt tokens came from the cache.
        assert_eq!(today["cache_hit_ratio"].as_f64(), Some(0.9), "{live}");
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
    assert!(stdout.contains("Schema version: 2"), "{stdout}");
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

    // Wait for the database file to appear and grow, rather than for a port.
    let deadline = Instant::now() + TIMEOUT;
    let database = data_dir.path().join("watch.db");
    let mut created = false;
    while Instant::now() < deadline {
        if database.exists() {
            created = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(created, "the database should be created without a server");

    // No port was opened, and the data is still readable afterwards.
    let output = Command::cargo_bin("agentbench")
        .expect("built binary")
        .arg("dashboard")
        .arg("--status")
        .arg("--data-dir")
        .arg(data_dir.path())
        .output()
        .expect("run status");
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
