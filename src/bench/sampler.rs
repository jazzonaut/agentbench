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
            while !worker_stop.load(Ordering::Relaxed) {
                if let Ok(mut output) = samples.lock() {
                    output.push(system::sample(&mut sys, started));
                }
                thread::sleep(SAMPLE_INTERVAL);
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
