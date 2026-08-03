//! Integer throughput on one core and across all logical processors.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::Result;
use std::{
    hint::black_box,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

/// Millions of xorshift iterations per second, single-threaded then saturated.
pub fn run(seconds: u64, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let duration = Duration::from_secs(seconds.max(1));
    let single = single_thread(duration, cancel);
    check_cancel(cancel)?;

    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let started = Instant::now();
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let cancel = cancel.clone();
            thread::spawn(move || spin(duration, cancel))
        })
        .collect();
    let total = handles.into_iter().filter_map(|h| h.join().ok()).sum();
    check_cancel(cancel)?;

    Ok(vec![
        single,
        catalog::CPU_MULTI_MOPS_S.scalar(total),
        catalog::CPU_MULTI_ELAPSED_MS.scalar(started.elapsed().as_secs_f64() * 1000.0),
    ])
}

/// Single-thread throughput alone, over an arbitrary duration.
///
/// The background prober's entry point. It cannot call [`run`], because that saturates every logical
/// processor: a probe is supposed to observe the machine four times an hour, not stall whatever the
/// user is doing on it. Sharing `spin` is what makes the probe's number comparable to the benchmark's
/// — same loop, same metric name, so a `diagnosis` threshold written for one applies to the other.
pub fn single_thread(duration: Duration, cancel: &Arc<AtomicBool>) -> Metric {
    catalog::CPU_SINGLE_MOPS_S.scalar(spin(duration, cancel.clone()))
}

/// Millions of iterations per second achieved by one thread over `duration`.
fn spin(duration: Duration, cancel: Arc<AtomicBool>) -> f64 {
    let started = Instant::now();
    let mut iterations = 0_u64;
    let mut state = 0x9e3779b97f4a7c15_u64;
    while started.elapsed() < duration && !cancel.load(std::sync::atomic::Ordering::Relaxed) {
        for _ in 0..10_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            iterations += 1;
        }
    }
    black_box(state);
    iterations as f64 / started.elapsed().as_secs_f64() / 1_000_000.0
}
