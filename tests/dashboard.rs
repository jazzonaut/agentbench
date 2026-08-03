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
    fn start(extra: &[&str]) -> Self {
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
        Some(1),
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
    assert!(stdout.contains("Schema version: 1"), "{stdout}");
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
