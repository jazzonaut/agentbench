//! The background metrics daemon.
//!
//! Layered strictly one way: `collect` produces records, `store` persists and queries them, `analysis`
//! draws conclusions from those queries, and `serve` presents the result. Nothing depends on `serve`,
//! neither `analysis` nor `serve` can write — both reach the database only through a read-only
//! [`Reader`] — so a handler cannot mutate history even by accident.
//!
//! [`Reader`]: store::Reader

pub mod analysis;
pub mod clock;
pub mod collect;
pub mod config;
pub mod maintenance;
pub mod marker;
pub mod platform;
pub mod serve;
pub mod settings;
pub mod store;
pub mod supervisor;

pub use config::WatchConfig;

use crate::system;
use anyhow::{Context, Result};
use clock::{Clock, SystemClock};
use serve::handlers::status::Status;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use store::{Level, Store};
use supervisor::{InstanceLock, ShutdownClock, Supervisor};

/// Where the daemon's startup narration goes.
///
/// A parameter rather than a `println!`, because the windowless build that shows a tray icon has no console
/// to print to: those lines would go nowhere and the "Press Ctrl+C to stop" among them would be a lie, since
/// there is no console to press it in.
pub enum Narrator {
    /// Printed to stdout, for a run started from a terminal.
    Stdout,
    /// Discarded. The tray build shows the same information in its tooltip and menu.
    Silent,
}

impl Narrator {
    fn say(&self, line: &str) {
        match self {
            Self::Stdout => println!("{line}"),
            Self::Silent => {}
        }
    }
}

/// Run the daemon until interrupted, narrating to stdout and handling Ctrl+C.
pub fn run(config: WatchConfig) -> Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));
    install_signal_handler(shutdown.clone());
    run_with(config, &Narrator::Stdout, shutdown)
}

/// Run the daemon against a caller-owned shutdown flag.
///
/// The tray build uses this so its Quit item and a Ctrl+C share one stopping path, the same arrangement
/// [`bench::run_with_cancel`] uses for `q` and Ctrl+C. Nothing here installs a signal handler: a caller that
/// owns the flag owns how it gets set.
///
/// Ordering matters. The instance lock and the socket are acquired *before* collection starts, so a
/// second daemon or an occupied port fails immediately and visibly rather than after the process
/// appears to have started successfully.
///
/// [`bench::run_with_cancel`]: crate::bench::run_with_cancel
pub fn run_with(config: WatchConfig, narrator: &Narrator, shutdown: Arc<AtomicBool>) -> Result<()> {
    config.ensure_loopback()?;
    let _lock = InstanceLock::acquire(&config.lock_path())?;

    let inventory = system::inventory(false);
    let store =
        Store::open(&config.database_path(), &inventory).context("open the watch database")?;
    let sink = store.sink();

    let server = if config.server.enabled {
        Some(serve::Server::bind(&config.server)?)
    } else {
        None
    };

    // Emptied at startup rather than when the first probe falls due, and whether or not probes are
    // enabled: a daemon killed mid-workload leaves files behind, and the promise that the location starts
    // empty should not depend on waiting a probe interval for it or on probing being switched on at all.
    // A failure here is reported and survived — it is a reason for the next probe to try again, not a
    // reason to refuse to collect.
    if let Err(error) = collect::probes::scratch::Scratch::clear(&config.collect, &config.data_dir)
    {
        sink.log(
            Level::Warn,
            "daemon",
            format!("could not clear the probe scratch directory: {error:#}"),
        );
    }

    let mut supervisor = Supervisor::with_shutdown(sink.clone(), shutdown.clone());

    // The sampler is polite: background CPU and I/O priority.
    let sampler_config = config.collect.clone();
    let sampler_sink = sink.clone();
    let sampler_shutdown = shutdown.clone();
    supervisor.spawn("sampler", true, move || {
        let clock = ShutdownClock::new(SystemClock, sampler_shutdown.clone());
        collect::sampler::run(&sampler_config, &clock, &sampler_sink);
    })?;

    if config.collect.probes_enabled {
        // `false`: the prober runs at normal priority, and that is load-bearing rather than an
        // oversight. A throttled measurement measures the throttle, and on Unix the throttle cannot be
        // lifted again without privileges — so a measured thread is *started* at normal priority and
        // there is no restore function anywhere for anyone to reach for.
        let prober_config = config.collect.clone();
        let prober_sink = sink.clone();
        let prober_shutdown = shutdown.clone();
        let scratch_parent = config.data_dir.clone();
        supervisor.spawn("prober", false, move || {
            let clock = ShutdownClock::new(SystemClock, prober_shutdown.clone());
            collect::probes::run(&prober_config, &scratch_parent, &clock, &prober_sink);
        })?;
    }

    // Retention is polite by nature and by priority: the work itself is bulk SQL on the writer's own
    // connection, and this thread does nothing but ask for it on a timer.
    let retention_config = config.retention.clone();
    let retention_sink = sink.clone();
    let retention_shutdown = shutdown.clone();
    supervisor.spawn("retention", true, move || {
        let clock = ShutdownClock::new(SystemClock, retention_shutdown.clone());
        maintenance::run(&retention_config, &clock, &retention_sink);
    })?;

    if config.sessions.enabled {
        // Reading transcripts is free in the sense that matters: Claude Code has already written
        // them, so this thread does no measuring, only accounting.
        let sessions_config = config.sessions.clone();
        let sessions_sink = sink.clone();
        let sessions_shutdown = shutdown.clone();
        let database = config.database_path();
        let machine = store.machine_id().to_string();
        supervisor.spawn("sessions", true, move || {
            let clock = ShutdownClock::new(SystemClock, sessions_shutdown.clone());
            match store::Reader::open(&database, machine.clone()) {
                Ok(reader) => {
                    collect::sessions::run(&sessions_config, &clock, &sessions_sink, &reader)
                }
                Err(error) => sessions_sink.log(
                    Level::Error,
                    "sessions",
                    format!("cannot open the database for reading: {error}"),
                ),
            }
        })?;
    }

    sink.log(
        Level::Info,
        "daemon",
        format!(
            "started; data dir {}, sampling every {:?} ({:?} when idle), transcripts {}",
            config.data_dir.display(),
            config.collect.sample_interval,
            config.collect.sample_interval_idle,
            if config.sessions.enabled {
                format!("every {:?}", config.sessions.poll_interval)
            } else {
                "disabled".to_string()
            }
        ),
    );

    if let Some(server) = server {
        narrator.say(&format!("AgentBench dashboard: {}", server.url()));
        narrator.say(&format!(
            "Data directory:       {}",
            config.data_dir.display()
        ));
        narrator.say("Press Ctrl+C to stop.");
        // Serving occupies this thread; collectors run behind it.
        server.serve(
            &store,
            &sink,
            shutdown.clone(),
            serve::Settings::from(&config.analysis).watching(store.writer_health()),
        );
    } else {
        narrator.say("AgentBench dashboard: collecting only (server disabled)");
        narrator.say(&format!(
            "Data directory:       {}",
            config.data_dir.display()
        ));
        narrator.say("Press Ctrl+C to stop.");
    }

    // Reached either because there was never a server or because the one there was gave up on its
    // listener. Both are the same situation from here: the collectors are the daemon, and the page is how
    // it is read. Falling through to shutdown — which is what used to happen — turned a broken socket into
    // a stopped daemon and logged it as "stopping HTTP server".
    let clock = ShutdownClock::new(SystemClock, shutdown.clone());
    while !shutdown.load(Ordering::Relaxed) {
        clock.sleep(config.collect.sample_interval);
    }

    sink.log(Level::Info, "daemon", "shutting down");
    drop(sink);
    supervisor.shutdown()?;
    store.shutdown()?;
    Ok(())
}

/// Read the current status without starting anything.
pub fn status(reader: &store::Reader, event_limit: usize) -> Result<Status> {
    serve::handlers::status::build(reader, event_limit, None)
}

/// Compare today against its trailing baseline without starting anything.
///
/// Reads the same database `--status` does, through the same analysis the dashboard's tiles use, so the
/// command line and the page can never reach different verdicts from the same rows.
pub fn verdicts(reader: &store::Reader, window_days: u32) -> Result<analysis::Comparisons> {
    analysis::today_against_baseline(reader, window_days)
}

/// Open the configured database read-only, explaining the common case of it not existing yet.
///
/// [`Reader::open`] rather than [`Store::open`]. Opening the store to obtain a connection that cannot
/// write would set pragmas, **run migrations**, insert a `machines` row and spawn a writer thread — and
/// the migration is not a wasted step but a dangerous one. Install a newer binary, run
/// `dashboard --status` while an older daemon is still collecting, and the schema is upgraded beneath
/// it; its prepared statements then run against a shape it does not know. That is exactly what
/// [`marker`] spends its doc comment avoiding, and the read path has no more right to do it.
///
/// A reader needs a path and a machine id, and [`system::machine_id`] is the cheap extract of the
/// second: the same hashed hostname [`system::inventory`] would have produced, without enumerating
/// every disk or spawning a child process to name the power source.
///
/// [`Reader::open`]: store::Reader::open
pub fn open_for_reading(config: &WatchConfig) -> Result<store::Reader> {
    let path = config.database_path();
    if !path.exists() {
        anyhow::bail!(
            "no watch database at {}; start the dashboard first",
            path.display()
        );
    }
    let reader = store::Reader::open(&path, system::machine_id())?;
    // Not migrating means the two ends of the version range have to be reported rather than fixed,
    // which is the same bargain `marker` makes. Both are ordinary rather than alarming: the first is a
    // status read racing a daemon that has only just created the file, and the second is an older
    // binary left on the PATH.
    let version: u32 = reader
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read the database schema version")?;
    if version == 0 {
        anyhow::bail!(
            "the watch database at {} has no schema yet; it is created when the daemon starts, so \
             try again in a moment",
            path.display()
        );
    }
    if version > store::migrations::target_version() {
        anyhow::bail!(
            "the watch database at {} was written by a newer AgentBench (schema v{version}, this \
             build understands v{}); upgrade to read it",
            path.display(),
            store::migrations::target_version()
        );
    }
    Ok(reader)
}

/// Whether another daemon currently holds the instance lock.
///
/// Probing by attempting to acquire it is the only portable way to ask, so the answer is inherently
/// a snapshot rather than a guarantee.
pub fn is_running(config: &WatchConfig) -> bool {
    InstanceLock::acquire(&config.lock_path()).is_err()
}

/// Erase every collected measurement, and report what was removed.
///
/// Done by deleting the database rather than by emptying its tables, for two reasons.
///
/// The first is that this is called from the control centre, which holds a read-only [`Reader`] on
/// purpose: the whole `store`/`Reader` split exists so that nothing outside the writer thread can
/// change history, and a screen that opened the database read-write to truncate it would be the one
/// exception that makes the guarantee meaningless.
///
/// The second is what a reset should mean. Removing the file takes `import_watermark` with it, so the
/// next daemon re-reads every transcript from the beginning and the session history — months of it,
/// derived from files still sitting on disk — comes back. A reset that emptied the measurement tables
/// but kept the watermarks would silently be irreversible for the one stream that did not have to be.
/// Probe and sample history is genuinely gone either way; that is what erasing means.
///
/// Refuses while a daemon holds the instance lock. On Windows the delete would fail anyway, with an
/// error about another process rather than an explanation.
///
/// [`Reader`]: store::Reader
pub fn reset_collected_data(config: &WatchConfig) -> Result<Vec<std::path::PathBuf>> {
    if is_running(config) {
        anyhow::bail!(
            "collection is running and has the database open; stop it first and try again"
        );
    }
    let database = config.database_path();
    let mut removed = Vec::new();
    // The write-ahead log and the shared-memory index are part of the database, not incidental files:
    // deleting only the first would leave a WAL that the next connection replays into a new database,
    // which is the one outcome worse than either keeping or removing all three.
    for path in [
        database.clone(),
        with_suffix(&database, "-wal"),
        with_suffix(&database, "-shm"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", path.display()));
            }
        }
    }
    Ok(removed)
}

/// `watch.db` plus `-wal`, as SQLite names its companions.
///
/// Appended to the file name rather than assembled from the stem, so a data directory whose name
/// contains a dot cannot end up with the suffix in the wrong place.
fn with_suffix(database: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = database.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    database.with_file_name(name)
}

/// Translate Ctrl+C into a cooperative shutdown request.
fn install_signal_handler(shutdown: Arc<AtomicBool>) {
    // A failure here means an existing handler is installed; collection still works, it just cannot
    // be stopped as gracefully, so it is not worth failing startup over.
    let _ = ctrlc::set_handler(move || shutdown.store(true, Ordering::Relaxed));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn config(dir: &std::path::Path) -> WatchConfig {
        WatchConfig::load(Some(dir.to_path_buf())).expect("defaults should load")
    }

    #[test]
    fn a_reset_removes_the_database_and_its_companions() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let database = config.database_path();
        fs::write(&database, b"not really a database").unwrap();
        fs::write(with_suffix(&database, "-wal"), b"log").unwrap();
        fs::write(with_suffix(&database, "-shm"), b"index").unwrap();
        // The configuration is not collected data and must survive.
        let settings = temp.path().join("watch.toml");
        assert!(settings.exists(), "load writes a default configuration");

        let removed = reset_collected_data(&config).expect("nothing holds the database");
        assert_eq!(removed.len(), 3, "{removed:?}");
        assert!(!database.exists());
        assert!(!with_suffix(&database, "-wal").exists());
        assert!(!with_suffix(&database, "-shm").exists());
        assert!(settings.exists(), "a reset must not erase the settings");
    }

    /// A first run has nothing to erase, which is a success and not an error.
    #[test]
    fn a_reset_with_no_database_yet_removes_nothing_and_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let removed = reset_collected_data(&config(temp.path())).expect("no database is fine");
        assert!(removed.is_empty());
    }

    #[test]
    fn a_reset_is_refused_while_a_daemon_holds_the_lock() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let database = config.database_path();
        fs::write(&database, b"not really a database").unwrap();

        let _lock = InstanceLock::acquire(&config.lock_path()).expect("the lock is free");
        let error = reset_collected_data(&config).expect_err("a running daemon must block a reset");
        assert!(
            format!("{error:#}").contains("stop it first"),
            "the message must say what to do: {error:#}"
        );
        assert!(database.exists(), "nothing may be removed after a refusal");
    }

    /// The suffix belongs on the file name, not on a stem split at the first dot.
    #[test]
    fn companions_are_named_after_the_whole_file() {
        let path = std::path::Path::new("C:/data.dir/watch.db");
        assert_eq!(
            with_suffix(path, "-wal"),
            std::path::Path::new("C:/data.dir/watch.db-wal")
        );
    }
}
