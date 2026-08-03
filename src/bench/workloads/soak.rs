//! Duration-filling sustained seek/read workload.
//!
//! When the measured phases finish before a preset's minimum duration, this keeps a light,
//! agent-shaped filesystem load running so that thermal, storage, memory, and background-scanner
//! observation windows stay comparable between machines.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::Result;
use std::{
    fs,
    hint::black_box,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

/// Number of 4 KiB files cycled through in a deliberately cache-unfriendly order.
const FILE_COUNT: usize = 512;

/// Stat and read small files in a strided order for `duration`.
pub fn run(root: &Path, duration: Duration, cancel: &Arc<AtomicBool>) -> Result<Metric> {
    if duration.is_zero() {
        return Ok(catalog::FS_SUSTAINED_SEEK_OPS_S.scalar(0.0));
    }
    eprintln!(
        "Sustained file-seek/resource sampling for {:.0}s to complete the preset duration",
        duration.as_secs_f64()
    );

    let directory = root.join("sustained-seek");
    fs::create_dir_all(&directory)?;
    let payload = vec![0x5a_u8; 4096];
    let paths: Vec<_> = (0..FILE_COUNT)
        .map(|index| directory.join(format!("seek-{index:04}.dat")))
        .collect();
    for path in &paths {
        fs::write(path, &payload)?;
    }

    let started = Instant::now();
    let mut operations = 0_u64;
    let mut index = 0_usize;
    while started.elapsed() < duration {
        if operations & 511 == 0 {
            check_cancel(cancel)?;
        }
        let path = &paths[(index.wrapping_mul(131)) % paths.len()];
        black_box(fs::metadata(path)?.len());
        let data = fs::read(path)?;
        black_box(data.first().copied());
        operations += 2;
        index = index.wrapping_add(1);
    }

    Ok(
        catalog::FS_SUSTAINED_SEEK_OPS_S
            .scalar(operations as f64 / started.elapsed().as_secs_f64()),
    )
}
