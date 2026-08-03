//! Platforms that have not been taught to install, edit `PATH`, or start at login.
//!
//! Every function here refuses with a reason rather than pretending to succeed. The equivalents exist on
//! Unix — a systemd user unit or a launchd agent for the login task, a shell profile line for `PATH` — but
//! both are conventions rather than an API, and guessing which shell a user's login reads is how a tool
//! ends up editing a file nobody sources. The seam is shaped to take them; see [`Autostart`], whose fields
//! describe intent rather than a `schtasks` invocation.
//!
//! [`Autostart`]: super::Autostart

use super::{Autostart, AutostartState, Support};
use anyhow::{Result, bail};
use std::path::PathBuf;

const REASON: &str = "only Windows is supported so far";

pub(crate) fn programs_dir() -> Result<PathBuf> {
    bail!("no per-user programs directory is known on this platform")
}

pub(crate) fn path_support() -> Support {
    Support::No(REASON)
}

pub(crate) fn read_user_path() -> Result<String> {
    bail!("editing the user's PATH is not supported on this platform")
}

pub(crate) fn write_user_path(_value: &str) -> Result<()> {
    bail!("editing the user's PATH is not supported on this platform")
}

pub(crate) fn autostart_support() -> Support {
    Support::No(REASON)
}

pub(crate) fn autostart_state() -> Result<AutostartState> {
    Ok(AutostartState::Unsupported(REASON))
}

pub(crate) fn enable_autostart(_autostart: &Autostart) -> Result<()> {
    bail!("starting at login is not supported on this platform")
}

pub(crate) fn disable_autostart() -> Result<bool> {
    Ok(false)
}

pub(crate) fn open(_target: &str) -> Result<()> {
    bail!("asking the shell to open something is not supported on this platform")
}

pub(crate) fn run_elevated(_program: &std::path::Path, _arguments: &str) -> Result<()> {
    bail!("elevation is not supported on this platform")
}

pub(crate) fn run_detached(_program: &std::path::Path, _arguments: &str) -> Result<()> {
    bail!("launching a program is not supported on this platform")
}
