//! Cooperative cancellation shared by the benchmark orchestrator and its workloads.

use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};

/// Fail the current phase if cancellation was requested.
///
/// Workloads call this between units of work so that pressing `q` or Ctrl+C unwinds through the
/// orchestrator, which drops the temporary directory on the way out.
pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("benchmark cancelled; temporary files were cleaned up");
    }
    Ok(())
}
