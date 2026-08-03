//! Background system sampling for the duration of a benchmark run.

use crate::{model::SystemSample, system};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Interval between samples during a benchmark run.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// How often the process table is re-enumerated, as against merely read.
///
/// The walk costs about 10 ms of a 500 ms interval on a Windows machine with a few hundred processes,
/// which is a 2% duty cycle spent on observation *during* the small-file and process phases - the two
/// measurements in the whole suite most sensitive to someone else using the machine. Every four
/// intervals instead makes the observer a quarter as visible, and costs only that `process_count` and
/// the scanner reading are up to two seconds stale. Neither is read as an instant: `diagnosis` takes
/// the maximum over the run.
const PROCESS_INTERVAL: Duration = Duration::from_secs(2);

/// Readings taken and discarded before the first recorded sample.
///
/// Two, because per-process CPU needs three refreshes before it is a measurement rather than a `0.0`,
/// as [`crate::process_tree::TreeUsage::cpu_percent`] explains. Without them the run's first sample
/// reported no scanner activity whatever was happening, and because `diagnosis` takes the *maximum*
/// scanner reading over a run, a phantom quiet sample was harmless while the missing real one was not.
/// The
/// whole-machine figure was wrong too, in the other direction: measured at 9.0% on the first sample
/// against 4.4% once settled, since its interval was the few milliseconds since the sampler started
/// rather than a full period.
const WARMUP_READINGS: usize = 2;

/// Owns the sampling thread and joins it on drop.
///
/// The guard exists so that an early return or a cancellation unwind cannot leave the sampler
/// running and holding the shared sample buffer.
pub(crate) struct SamplerGuard {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SamplerGuard {
    pub(crate) fn spawn(
        stop: Arc<AtomicBool>,
        samples: Arc<Mutex<Vec<SystemSample>>>,
        started: Instant,
    ) -> Self {
        let worker_stop = stop.clone();
        let handle = thread::spawn(move || {
            let mut sys = sysinfo::System::new_all();
            // Warm up before recording anything, and honour the shutdown flag while doing it: a run
            // cancelled during the warm-up must not keep the thread alive for the full wait.
            for _ in 0..WARMUP_READINGS {
                if worker_stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
                system::refresh_for_sample(&mut sys);
            }

            let mut since_processes = PROCESS_INTERVAL;
            while !worker_stop.load(Ordering::Relaxed) {
                if since_processes >= PROCESS_INTERVAL {
                    system::refresh_processes_for_sample(&mut sys);
                    since_processes = Duration::ZERO;
                }
                system::refresh_machine(&mut sys);
                if let Ok(mut output) = samples.lock() {
                    output.push(system::sample_from(&sys, started));
                }
                thread::sleep(SAMPLE_INTERVAL);
                since_processes += SAMPLE_INTERVAL;
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SamplerGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
