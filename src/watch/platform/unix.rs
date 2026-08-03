//! Unix implementations: XDG/Application Support paths, `flock`, and nice/idle-I/O scheduling.

use super::Capability;
use anyhow::{Context, Result, bail};
use std::{env, fs::File, os::unix::io::AsRawFd, path::PathBuf};

/// Nice increment applied to background collector threads.
const BACKGROUND_NICE: libc::c_int = 10;

pub(super) fn default_data_dir() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        if let Some(home) = env::var_os("HOME") {
            return Ok(PathBuf::from(home)
                .join("Library")
                .join("Application Support"));
        }
    }
    if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local").join("share"));
    }
    bail!("neither XDG_DATA_HOME nor HOME is set; pass AGENTBENCH_DATA_DIR explicitly")
}

pub(super) fn try_lock_exclusive(file: &File) -> Result<bool> {
    // SAFETY: `fd` is a valid descriptor owned by `file` for the duration of the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(false),
        _ => Err(error).context("take an exclusive lock on the daemon lock file"),
    }
}

/// Lower scheduling priority for the calling thread.
///
/// On Linux `setpriority(PRIO_PROCESS, 0, …)` applies to the calling *thread*, which is the
/// granularity wanted. Elsewhere it may affect the whole process; the fallback is still an
/// improvement over competing at normal priority, and I/O priority is left untouched because no
/// portable interface exists.
pub(super) fn set_current_thread_background() -> Capability {
    set_nice(BACKGROUND_NICE)
}

pub(super) fn clear_current_thread_background() -> Capability {
    set_nice(0)
}

fn set_nice(value: libc::c_int) -> Capability {
    // SAFETY: setpriority with PRIO_PROCESS and who=0 targets the caller and takes no pointers.
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, value) };
    if result == 0 {
        Capability::Applied
    } else {
        Capability::Unsupported(format!(
            "setpriority({value}) was refused: {}",
            std::io::Error::last_os_error()
        ))
    }
}
