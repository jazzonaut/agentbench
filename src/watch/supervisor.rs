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
    any::Any,
    fs::File,
    panic::AssertUnwindSafe,
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

/// Granularity at which a shutdown request is noticed during a long sleep.
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);

/// Sleep for `total`, returning early if shutdown is requested.
///
/// Workers get this from [`ShutdownClock`]; the supervisor's own restart backoff used to be a bare
/// `thread::sleep`, so Ctrl+C arriving just after a worker died waited the full five seconds before the
/// process would exit — a delay with no purpose, since the worker being waited for is not going to start.
fn sleep_unless_shutdown(shutdown: &AtomicBool, total: Duration) {
    let mut remaining = total;
    while !remaining.is_zero() && !shutdown.load(Ordering::Relaxed) {
        let slice = remaining.min(SHUTDOWN_POLL);
        thread::sleep(slice);
        remaining -= slice;
    }
}

/// The message carried by a panic, for the two payload types `panic!` actually produces.
///
/// Anything else is reported as unreadable rather than guessed at: the point of the line is to name
/// the fault, and a payload nobody can print is itself worth saying out loud.
///
/// Shared with [`serve`], which catches a panic for the same reason and has to describe it the same way. Two
/// copies of this would eventually disagree about which payload types are worth downcasting.
///
/// [`serve`]: crate::watch::serve
pub(crate) fn describe(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "a payload of an unprintable type"
    }
}

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
            poll: SHUTDOWN_POLL,
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
        Self::with_shutdown(sink, Arc::new(AtomicBool::new(false)))
    }

    /// Take a shutdown flag the caller already holds.
    ///
    /// The tray build needs this: its Quit menu item has to be able to stop collection, and it cannot reach
    /// a flag the supervisor created for itself.
    pub fn with_shutdown(sink: Sink, shutdown: Arc<AtomicBool>) -> Self {
        Self {
            shutdown,
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

    /// Spawn a worker that runs `body` and is restarted if it stops for any reason.
    ///
    /// `background` requests reduced CPU and I/O priority for the worker. Pass `false` for anything
    /// whose timing is being measured.
    ///
    /// A panic is caught here rather than allowed to unwind the thread. An earlier version let it
    /// unwind and relied on the join in [`shutdown`] to report it, which meant an index panic somewhere
    /// inside process enumeration took one stream out for the life of the process and said so only at
    /// exit — potentially days later, with the dashboard looking healthy throughout. A panicking worker
    /// and a returning one are the same fault from the outside, so they take the same path.
    ///
    /// [`shutdown`]: Supervisor::shutdown
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
                    // `AssertUnwindSafe` is honest here: the body owns everything it touches, and what
                    // it shares — the sink and the shutdown flag — carries no invariant a half-finished
                    // iteration could break.
                    let body = body.clone();
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(body));
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    // Reaching here means the worker stopped without being asked to.
                    let (level, what) = match &outcome {
                        Ok(()) => (Level::Warn, "stopped unexpectedly".to_string()),
                        Err(payload) => {
                            (Level::Error, format!("panicked: {}", describe(&**payload)))
                        }
                    };
                    sink.log(
                        level,
                        name,
                        format!(
                            "worker {what}; restarting in {}s",
                            RESTART_BACKOFF.as_secs()
                        ),
                    );
                    sleep_unless_shutdown(&shutdown, RESTART_BACKOFF);
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
    ///
    /// A backstop rather than the main path, since [`spawn`] catches a panic in the worker body and
    /// restarts it. What reaches here is a panic outside that body — in the priority prologue, or in
    /// the sink — which is not something a restart would survive either.
    ///
    /// [`spawn`]: Supervisor::spawn
    pub fn shutdown(mut self) -> Result<()> {
        let panicked = self.stop();
        if !panicked.is_empty() {
            bail!("worker thread(s) panicked: {}", panicked.join(", "));
        }
        Ok(())
    }

    /// Ask every worker to stop and wait for it, naming the ones that panicked.
    ///
    /// Idempotent, because both [`shutdown`] and [`Drop`] call it and the first may already have run.
    ///
    /// [`shutdown`]: Supervisor::shutdown
    fn stop(&mut self) -> Vec<&'static str> {
        self.request_shutdown();
        let mut panicked = Vec::new();
        for worker in &mut self.workers {
            if let Some(handle) = worker.handle.take()
                && handle.join().is_err()
            {
                panicked.push(worker.name);
            }
        }
        panicked
    }
}

/// Stop and join the workers however this supervisor goes out of scope.
///
/// Not a tidy-up. The worker threads hold [`Sink`] clones, and the writer thread behind that sink only ends
/// when *every* sender has been dropped — so `Store`'s own `Drop`, which waits for the writer, cannot finish
/// while a worker is still alive. Leaving the handles to be dropped unjoined therefore did not detach the
/// workers harmlessly: it hung the process.
///
/// The path that showed it was a panic in an HTTP handler. Serving occupies the main thread, so the panic
/// unwound out of `watch::run_with` without reaching `shutdown()`; the workers kept collecting, the writer
/// could never see its channel close, and the unwind blocked for ever inside `Store::drop` still holding the
/// instance lock — a daemon that answered nothing, exited never, and let no replacement start. The handler
/// panic has its own boundary now, and this makes any *other* unexpected exit from `run_with` end the process
/// rather than wedge it.
impl Drop for Supervisor {
    fn drop(&mut self) {
        // The names are discarded rather than reported. A panic reaching here is a panic outside the worker
        // body — `spawn` catches the ones inside it — and the sink this would complain through is being torn
        // down in the same breath, so there is nowhere left for the complaint to be written.
        let _ = self.stop();
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

    /// Longest a report is waited for. Generous: the worker logs within microseconds of stopping, and the
    /// only thing this bound protects against is hanging the suite if it never does.
    const REPORT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Wait for a worker's report to arrive, rather than inferring it from a counter.
    ///
    /// The distinction is the whole point. A worker that has been asked to shut down deliberately skips its
    /// report — stopping is not a fault worth logging — so a test that watched a counter the body bumps
    /// *before* stopping could reach `shutdown()` first, send the worker down that branch, and then find
    /// nothing logged. That is a race in the test rather than the code, and on a loaded CI runner it lost.
    fn wait_for_report(
        receiver: &std::sync::mpsc::Receiver<crate::watch::store::Record>,
        source: &str,
        level: Level,
        needle: &str,
    ) -> bool {
        let deadline = std::time::Instant::now() + REPORT_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(crate::watch::store::Record::Event(event)) => {
                    if event.source == source
                        && event.level == level
                        && event.message.contains(needle)
                    {
                        return true;
                    }
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        false
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
        let warned = wait_for_report(&receiver, "flaky", Level::Warn, "stopped unexpectedly");
        assert!(
            runs.load(Ordering::Relaxed) >= 1,
            "the body should have run at least once"
        );
        supervisor.shutdown().unwrap();
        assert!(warned, "an early return must be logged");
    }

    /// Ctrl+C during a restart backoff must not wait the backoff out.
    ///
    /// A worker that returns immediately puts its thread in the backoff almost at once, which is exactly
    /// when a user is most likely to give up and interrupt. The bare `thread::sleep` this replaced held
    /// shutdown for the full five seconds; the assertion is loose enough to survive a loaded CI runner and
    /// still nowhere near that.
    #[test]
    fn shutdown_during_a_restart_backoff_is_not_delayed_by_it() {
        let (sink, _receiver) = sink();
        let mut supervisor = Supervisor::new(sink);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        supervisor
            .spawn("brief", false, move || {
                counter.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        for _ in 0..500 {
            if runs.load(Ordering::Relaxed) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let started = std::time::Instant::now();
        supervisor.shutdown().unwrap();
        assert!(
            started.elapsed() < RESTART_BACKOFF,
            "shutdown waited {:?} of a {RESTART_BACKOFF:?} backoff",
            started.elapsed()
        );
    }

    /// Dropping a supervisor stops its workers, which is what keeps an unwind from hanging the process.
    ///
    /// The worker holds a sink clone; the writer thread ends only when the last sender is gone. So a
    /// supervisor that dropped without joining left `Store::drop` waiting on a channel that could never
    /// close — which is how a panic in an HTTP handler turned into a daemon that never exited and never
    /// released its instance lock. The assertion is that the thread is *finished* after the drop, not merely
    /// that the drop returned.
    #[test]
    fn dropping_a_supervisor_stops_and_joins_its_workers() {
        let (sink, _receiver) = sink();
        let running = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        {
            let mut supervisor = Supervisor::new(sink);
            let flag = supervisor.shutdown_flag();
            let entered = running.clone();
            let left = stopped.clone();
            supervisor
                .spawn("dropped", false, move || {
                    entered.store(true, Ordering::Relaxed);
                    while !flag.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(1));
                    }
                    left.store(true, Ordering::Relaxed);
                })
                .unwrap();
            for _ in 0..500 {
                if running.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(running.load(Ordering::Relaxed), "the worker never started");
        }
        assert!(
            stopped.load(Ordering::Relaxed),
            "the drop must have joined the worker, not detached it"
        );
    }

    /// The failure that used to be invisible until the process exited.
    ///
    /// The default panic hook still prints a backtrace to stderr while this runs, which is noise in the
    /// test output and the right behaviour in a daemon.
    #[test]
    fn a_worker_that_panics_is_reported_and_the_thread_survives() {
        let (sink, receiver) = sink();
        let mut supervisor = Supervisor::new(sink);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        supervisor
            .spawn("panicky", false, move || {
                counter.fetch_add(1, Ordering::Relaxed);
                panic!("process enumeration went wrong");
            })
            .unwrap();
        let reported = wait_for_report(
            &receiver,
            "panicky",
            Level::Error,
            "process enumeration went wrong",
        );
        assert!(
            runs.load(Ordering::Relaxed) >= 1,
            "the body should have run at least once"
        );
        supervisor
            .shutdown()
            .expect("a caught panic must not surface as a panicked thread");
        assert!(reported, "a panic must be logged with its own message");
    }
}
