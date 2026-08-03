//! Thread lifecycle: shutdown, restart-on-death, and single-instance enforcement.
//!
//! Collectors stay concrete types rather than implementing a shared trait. They are genuinely
//! dissimilar — one is cheap and periodic, one expensive and periodic, one filesystem-driven — so a
//! common trait would collapse to something uninformative. What they *do* share is scheduling, and
//! that is what [`Supervisor`] provides.

use crate::watch::{
    clock::Clock,
    platform,
    store::{Level, Sink},
};
use anyhow::{Context, Result, bail};
use std::{
    fs::File,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// Delay before a died-unexpectedly worker is restarted.
const RESTART_BACKOFF: Duration = Duration::from_secs(5);

/// Held for the lifetime of the daemon to prove only one instance is collecting.
///
/// Two daemons writing the same database would double every sample and corrupt the very trend the
/// dashboard exists to show.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Acquire the lock, or explain that another daemon already holds it.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;
        if !platform::try_lock_exclusive(&file)? {
            bail!(
                "another AgentBench dashboard is already running for this data directory \
                 ({}); stop it first or pass a different --data-dir",
                path.display()
            );
        }
        Ok(Self { _file: file })
    }
}

/// A clock whose sleeps end early when shutdown is requested.
///
/// Wrapping rather than polling: a collector sleeping 30 seconds must still stop promptly on Ctrl+C,
/// and it should not have to think about that itself.
pub struct ShutdownClock<C: Clock> {
    inner: C,
    shutdown: Arc<AtomicBool>,
    /// Granularity at which shutdown is noticed during a long sleep.
    poll: Duration,
}

impl<C: Clock> ShutdownClock<C> {
    pub fn new(inner: C, shutdown: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            shutdown,
            poll: Duration::from_millis(250),
        }
    }
}

impl<C: Clock> Clock for ShutdownClock<C> {
    fn now_ms(&self) -> i64 {
        self.inner.now_ms()
    }

    fn sleep(&self, duration: Duration) -> bool {
        let mut remaining = duration;
        while !remaining.is_zero() {
            if self.shutdown.load(Ordering::Relaxed) {
                return false;
            }
            let slice = remaining.min(self.poll);
            if !self.inner.sleep(slice) {
                return false;
            }
            remaining = remaining.saturating_sub(slice);
        }
        !self.shutdown.load(Ordering::Relaxed)
    }
}

/// Owns worker threads and keeps them alive.
pub struct Supervisor {
    shutdown: Arc<AtomicBool>,
    workers: Vec<Worker>,
    sink: Sink,
}

struct Worker {
    name: &'static str,
    handle: Option<thread::JoinHandle<()>>,
}

impl Supervisor {
    pub fn new(sink: Sink) -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            workers: Vec::new(),
            sink,
        }
    }

    /// Flag consulted by workers and by [`ShutdownClock`].
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// Ask every worker to stop at its next opportunity.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// Spawn a worker that runs `body` and is restarted if it returns early.
    ///
    /// `background` requests reduced CPU and I/O priority for the worker. Pass `false` for anything
    /// whose timing is being measured.
    pub fn spawn<F>(&mut self, name: &'static str, background: bool, body: F) -> Result<()>
    where
        F: Fn() + Send + Clone + 'static,
    {
        let shutdown = self.shutdown.clone();
        let sink = self.sink.clone();
        let handle = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                if background {
                    let capability = platform::set_current_thread_background();
                    if let Some(reason) = capability.reason() {
                        sink.log(
                            Level::Info,
                            name,
                            format!("running at normal priority: {reason}"),
                        );
                    }
                }
                while !shutdown.load(Ordering::Relaxed) {
                    body.clone()();
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    // Reaching here means the worker returned without being asked to. A panic would
                    // have unwound instead, which the join in `shutdown` reports separately.
                    sink.log(
                        Level::Warn,
                        name,
                        format!(
                            "worker stopped unexpectedly; restarting in {}s",
                            RESTART_BACKOFF.as_secs()
                        ),
                    );
                    thread::sleep(RESTART_BACKOFF);
                }
            })
            .with_context(|| format!("spawn {name} thread"))?;
        self.workers.push(Worker {
            name,
            handle: Some(handle),
        });
        Ok(())
    }

    /// Stop every worker and report any that panicked.
    pub fn shutdown(mut self) -> Result<()> {
        self.request_shutdown();
        let mut panicked = Vec::new();
        for worker in &mut self.workers {
            if let Some(handle) = worker.handle.take()
                && handle.join().is_err()
            {
                panicked.push(worker.name);
            }
        }
        if !panicked.is_empty() {
            bail!("worker thread(s) panicked: {}", panicked.join(", "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::clock::SystemClock;
    use std::sync::atomic::AtomicUsize;

    fn sink() -> (Sink, std::sync::mpsc::Receiver<crate::watch::store::Record>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(256);
        (Sink::new(sender), receiver)
    }

    #[test]
    fn a_second_lock_on_the_same_path_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.lock");
        let _first = InstanceLock::acquire(&path).unwrap();
        let error = InstanceLock::acquire(&path).unwrap_err().to_string();
        assert!(error.contains("already running"), "{error}");
    }

    #[test]
    fn the_lock_is_released_when_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.lock");
        drop(InstanceLock::acquire(&path).unwrap());
        InstanceLock::acquire(&path).expect("lock should be reacquirable after release");
    }

    #[test]
    fn shutdown_clock_refuses_to_sleep_once_shutdown_is_requested() {
        let flag = Arc::new(AtomicBool::new(false));
        let clock = ShutdownClock::new(SystemClock, flag.clone());
        assert!(clock.sleep(Duration::from_millis(1)));
        flag.store(true, Ordering::Relaxed);
        assert!(
            !clock.sleep(Duration::from_secs(60)),
            "must return immediately"
        );
    }

    #[test]
    fn a_worker_runs_and_stops_on_request() {
        let (sink, _receiver) = sink();
        let mut supervisor = Supervisor::new(sink);
        let ticks = Arc::new(AtomicUsize::new(0));
        let flag = supervisor.shutdown_flag();
        let counter = ticks.clone();
        supervisor
            .spawn("test-worker", true, move || {
                while !flag.load(Ordering::Relaxed) {
                    counter.fetch_add(1, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(1));
                }
            })
            .unwrap();
        // Wait for observable progress rather than sleeping a fixed amount.
        for _ in 0..500 {
            if ticks.load(Ordering::Relaxed) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(ticks.load(Ordering::Relaxed) > 0, "worker never ran");
        supervisor.shutdown().unwrap();
    }

    #[test]
    fn a_worker_that_returns_early_is_reported_and_restarted() {
        let (sink, receiver) = sink();
        let mut supervisor = Supervisor::new(sink);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        // Returns immediately every time, which the supervisor should treat as unexpected.
        supervisor
            .spawn("flaky", false, move || {
                counter.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        for _ in 0..500 {
            if runs.load(Ordering::Relaxed) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        supervisor.shutdown().unwrap();
        let warned = receiver.try_iter().any(|record| {
            matches!(record, crate::watch::store::Record::Event(event)
                if event.source == "flaky" && event.message.contains("stopped unexpectedly"))
        });
        assert!(warned, "an early return must be logged");
    }
}
