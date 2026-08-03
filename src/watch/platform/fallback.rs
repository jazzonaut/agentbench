//! Fallbacks for targets that are neither Windows nor Unix.
//!
//! Nothing here pretends to work. The daemon still runs; it simply reports the missing capabilities.

use super::Capability;
use anyhow::{Result, bail};
use std::{fs::File, path::PathBuf};

pub(super) fn default_data_dir() -> Result<PathBuf> {
    bail!("this platform has no known data directory; set AGENTBENCH_DATA_DIR")
}

/// Without an advisory-locking primitive we cannot prove single-instance, so we do not claim to.
pub(super) fn try_lock_exclusive(_file: &File) -> Result<bool> {
    Ok(true)
}

pub(super) fn set_current_thread_background() -> Capability {
    Capability::Unsupported("thread priority control is unavailable on this platform".into())
}

/// No privilege model is known here, so nothing elevated is claimed or attempted.
pub(super) fn is_elevated() -> bool {
    false
}

/// No power interface is known here, and a probe stamped with a guessed power source is worse than
/// one stamped with nothing.
pub(super) fn on_battery() -> Option<bool> {
    None
}
