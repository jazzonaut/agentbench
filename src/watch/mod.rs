//! The background metrics daemon.
//!
//! Layered strictly one way: `collect` produces records, `store` persists and queries them, `serve`
//! presents them. Nothing depends on `serve`, and `serve` reaches the database only through a
//! read-only [`Reader`], so a handler cannot write even by accident.
//!
//! [`Reader`]: store::Reader

pub mod clock;
pub mod collect;
pub mod config;
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

    let mut supervisor = Supervisor::new(sink.clone());
    let shutdown = supervisor.shutdown_flag();
    install_signal_handler(shutdown.clone());

    // The sampler is polite: background CPU and I/O priority. Probes, in a later phase, will not be,
    // because a throttled measurement measures the throttle.
    let sampler_config = config.collect.clone();
    let sampler_sink = sink.clone();
    let sampler_shutdown = shutdown.clone();
    supervisor.spawn("sampler", true, move || {
        let clock = ShutdownClock::new(SystemClock, sampler_shutdown.clone());
        collect::sampler::run(&sampler_config, &clock, &sampler_sink);
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

    match server {
        Some(server) => {
            println!("AgentBench dashboard: {}", server.url());
            println!("Data directory:       {}", config.data_dir.display());
            println!("Press Ctrl+C to stop.");
            // Serving occupies this thread; collectors run behind it.
            server.serve(&store, &sink, shutdown.clone());
        }
        None => {
            println!("AgentBench dashboard: collecting only (server disabled)");
            println!("Data directory:       {}", config.data_dir.display());
            println!("Press Ctrl+C to stop.");
            let clock = ShutdownClock::new(SystemClock, shutdown.clone());
            while !shutdown.load(Ordering::Relaxed) {
                clock.sleep(config.collect.sample_interval);
            }
        }
    }

    sink.log(Level::Info, "daemon", "shutting down");
    drop(sink);
    supervisor.shutdown()?;
    store.shutdown()?;
    Ok(())
}

/// Read the current status without starting anything.
pub fn status(config: &WatchConfig, event_limit: usize) -> Result<Status> {
    let path = config.database_path();
    if !path.exists() {
        anyhow::bail!(
            "no watch database at {}; start the dashboard first",
            path.display()
        );
    }
    let inventory = system::inventory(false);
    let store = Store::open(&path, &inventory)?;
    let reader = store.reader()?;
    serve::handlers::status::build(&reader, event_limit)
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
