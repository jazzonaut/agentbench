//! Windows: `%LOCALAPPDATA%\Programs`, `HKCU\Environment`, and a logon task.

mod environment;
mod launch;
mod task;

pub(super) use environment::{path_support, read_user_path, write_user_path};
pub(super) use launch::{open, run_detached, run_elevated};
pub(super) use task::{autostart_state, autostart_support, disable_autostart, enable_autostart};

use anyhow::{Result, bail};
use std::{env, path::PathBuf};

/// Where per-user programs are installed without administrator rights.
///
/// `%LOCALAPPDATA%\Programs` is the convention Windows itself uses for per-user installs, and it needs no
/// elevation — which is the point, since installing is meant to be a checkbox rather than a prompt.
pub(super) fn programs_dir() -> Result<PathBuf> {
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(local).join("Programs"));
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile)
            .join("AppData")
            .join("Local")
            .join("Programs"));
    }
    bail!("neither LOCALAPPDATA nor USERPROFILE is set, so there is nowhere to install to")
}
