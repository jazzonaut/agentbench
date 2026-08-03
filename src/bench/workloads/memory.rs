//! Bounded memory write and read throughput.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::{Context, Result};
use std::{
    hint::black_box,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

/// Fill and then sample a buffer of exactly `size` bytes.
///
/// The allocation is fallible on purpose: presets scale the buffer to a fraction of installed RAM,
/// and a machine under memory pressure should report a clear error rather than invite the OOM killer.
pub fn run(size: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let mut buffer = Vec::<u8>::new();
    buffer
        .try_reserve_exact(size)
        .context("reserve memory benchmark buffer")?;
    buffer.resize(size, 0);

    let started = Instant::now();
    for (index, byte) in buffer.iter_mut().enumerate() {
        if index % (16 << 20) == 0 {
            check_cancel(cancel)?;
        }
        *byte = (index as u8).wrapping_mul(31);
    }
    let write = size as f64 / started.elapsed().as_secs_f64() / 1_073_741_824.0;

    let started = Instant::now();
    let checksum: u64 = buffer.iter().step_by(64).map(|v| *v as u64).sum();
    black_box(checksum);
    let read = size as f64 / started.elapsed().as_secs_f64() / 1_073_741_824.0;

    Ok(vec![
        catalog::MEMORY_WRITE_GIB_S.scalar(write),
        catalog::MEMORY_READ_GIB_S.scalar(read),
    ])
}
