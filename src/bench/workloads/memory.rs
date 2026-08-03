//! Bounded memory write and read throughput.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::{Context, Result};
use std::{
    hint::black_box,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

/// How much is written between two checks for cancellation.
///
/// Big enough that the check is free — a few hundred branches over a gigabyte — and small enough that
/// Ctrl+C is still answered promptly, since 16 MiB of stores takes single-digit milliseconds on any
/// machine this tool is worth running on.
const CANCEL_CHUNK: usize = 16 << 20;

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

    // The cancel check is outside the byte loop, and that is the measurement rather than a tidiness
    // preference. With `if index % … == 0` inside it, the store cannot be vectorised, and what this
    // workload reported was the branch rather than the machine: measured on a release build at
    // 2.4 GiB/s in that shape against roughly 28 in this one, at both 64 MiB and 512 MiB, writing
    // byte-for-byte identical output. The figure this workload publishes moved by an order of
    // magnitude when it was fixed; see the changelog entry.
    let started = Instant::now();
    for (chunk_index, chunk) in buffer.chunks_mut(CANCEL_CHUNK).enumerate() {
        check_cancel(cancel)?;
        let base = chunk_index * CANCEL_CHUNK;
        for (offset, byte) in chunk.iter_mut().enumerate() {
            *byte = ((base + offset) as u8).wrapping_mul(31);
        }
    }
    let write = size as f64 / started.elapsed().as_secs_f64() / 1_073_741_824.0;

    // Cache-line-granular, not a stream: one byte in every 64 is touched, so this reports the rate at
    // which the machine can *reach* `size` bytes rather than the rate at which it can move them. That
    // is why it can exceed the theoretical bandwidth of the memory bus, and why it is charted beside
    // the write figure rather than compared to it.
    let started = Instant::now();
    let checksum: u64 = buffer.iter().step_by(64).map(|v| *v as u64).sum();
    black_box(checksum);
    let read = size as f64 / started.elapsed().as_secs_f64() / 1_073_741_824.0;

    Ok(vec![
        catalog::MEMORY_WRITE_GIB_S.scalar(write),
        catalog::MEMORY_READ_GIB_S.scalar(read),
    ])
}
