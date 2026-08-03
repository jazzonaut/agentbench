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

/// Run the daemon until interrupted.
///
/// Ordering matters. The instance lock and the socket are acquired *before* collection starts, so a
/// second daemon or an occupied port fails immediately and visibly rather than after the process
/// appears to have started successfully.
pub fn run(config: WatchConfig) -> Result<()> {
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

    let mut supervisor = Supervisor::new(sink.clone());
    let shutdown = supervisor.shutdown_flag();
    install_signal_handler(shutdown.clone());

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
        println!("AgentBench dashboard: {}", server.url());
        println!("Data directory:       {}", config.data_dir.display());
        println!("Press Ctrl+C to stop.");
        // Serving occupies this thread; collectors run behind it.
        server.serve(
            &store,
            &sink,
            shutdown.clone(),
            serve::Settings::from(&config.analysis).watching(store.writer_health()),
        );
    } else {
        println!("AgentBench dashboard: collecting only (server disabled)");
        println!("Data directory:       {}", config.data_dir.display());
        println!("Press Ctrl+C to stop.");
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

/// Translate Ctrl+C into a cooperative shutdown request.
fn install_signal_handler(shutdown: Arc<AtomicBool>) {
    // A failure here means an existing handler is installed; collection still works, it just cannot
    // be stopped as gracefully, so it is not worth failing startup over.
    let _ = ctrlc::set_handler(move || shutdown.store(true, Ordering::Relaxed));
}
