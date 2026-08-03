//! Platforms with no notification area this knows how to use.

use super::{Item, Status};
use anyhow::{Result, bail};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) fn is_supported() -> bool {
    false
}

pub(super) fn run(
    _shutdown: Arc<AtomicBool>,
    _status: Status,
    _on: impl FnMut(Item),
) -> Result<()> {
    bail!("a tray icon is not supported on this platform")
}
