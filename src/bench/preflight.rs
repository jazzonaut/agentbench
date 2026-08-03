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
    let available = available_space_for(target).unwrap_or(u64::MAX);
    if available < required {
        bail!(
            "insufficient free space: need at least {:.1} GiB free beneath {}",
            required as f64 / 1_073_741_824.0,
            target.display()
        );
    }
    Ok(())
}

/// Free space on the most specific mounted volume containing `path`.
///
/// Returns `None` when no mount point matches, which the caller treats as unknown rather than empty.
fn available_space_for(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}
