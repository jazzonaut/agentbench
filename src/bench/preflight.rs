//! Checks performed before any workload writes to the target volume.

use crate::bench::preset::Limits;
use anyhow::{Result, bail};
use std::path::Path;

/// Refuse to start unless at least twice the generated working set is free.
///
/// Twice, not once: the sequential file and the small-file tree coexist inside the same temporary
/// directory, and filling a volume is the one failure mode that damages something other than the run.
pub(crate) fn ensure_free_space(target: &Path, limits: &Limits) -> Result<()> {
    let required = limits.disk_working_set.saturating_mul(2);
    let available = crate::system::available_space(target).unwrap_or(u64::MAX);
    if available < required {
        bail!(
            "insufficient free space: need at least {:.1} GiB free beneath {}",
            required as f64 / 1_073_741_824.0,
            target.display()
        );
    }
    Ok(())
}
