//! Process launch latency, measured against AgentBench's own hidden no-op subcommand.

use crate::{metrics::catalog, model::Metric};
use anyhow::{Result, bail};
use std::{process::Command, time::Instant};

/// Launch and reap ten minimal child processes.
///
/// Spawning our own executable keeps the measurement comparable across machines: no dependency on
/// which shells or system utilities happen to be installed, and the image is already in cache.
pub fn run() -> Result<Vec<Metric>> {
    let executable = std::env::current_exe()?;
    let mut times = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let status = Command::new(&executable).arg("internal-noop").status()?;
        if !status.success() {
            bail!("internal process benchmark failed");
        }
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(vec![catalog::PROCESS_SPAWN_MS.distribution(&times)])
}
