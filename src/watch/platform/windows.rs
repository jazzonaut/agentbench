//! Windows implementations: `%LOCALAPPDATA%`, `LockFileEx`, and per-thread background mode.

use super::Capability;
use anyhow::{Context, Result, bail};
use std::{env, fs::File, os::windows::io::AsRawHandle, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{ERROR_LOCK_VIOLATION, GetLastError, HANDLE},
    Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx},
    System::{
        IO::OVERLAPPED,
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
        Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN},
    },
};

/// `ACLineStatus` value meaning "running on battery".
const AC_LINE_OFFLINE: u8 = 0;

/// `ACLineStatus` value meaning "the system cannot tell".
const AC_LINE_UNKNOWN: u8 = 255;

pub(super) fn default_data_dir() -> Result<PathBuf> {
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(local));
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile).join("AppData").join("Local"));
    }
    bail!("neither LOCALAPPDATA nor USERPROFILE is set; pass AGENTBENCH_DATA_DIR explicitly")
}

pub(super) fn try_lock_exclusive(file: &File) -> Result<bool> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: `handle` is a valid file handle owned by `file` for the duration of the call, and
    // `overlapped` is a correctly sized, zeroed structure that LockFileEx may write to.
    let locked = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(true);
    }
    // SAFETY: called immediately after the failed call on the same thread.
    let error = unsafe { GetLastError() };
    if error == ERROR_LOCK_VIOLATION {
        return Ok(false);
    }
    Err(std::io::Error::from_raw_os_error(error as i32))
        .context("take an exclusive lock on the daemon lock file")
}

/// `THREAD_MODE_BACKGROUND_BEGIN` lowers CPU *and* I/O priority for this thread alone, which is
/// exactly the granularity needed: the sampler can be polite while the prober stays honest.
///
/// Windows can undo this with `THREAD_MODE_BACKGROUND_END`, but no counterpart is exposed, because
/// Unix cannot and a capability that exists on one platform only would be a trap for the caller.
pub(super) fn set_current_thread_background() -> Capability {
    apply(THREAD_MODE_BACKGROUND_BEGIN, "enter")
}

/// `GetSystemPowerStatus` is a single call into the power manager, cheap enough to ask immediately
/// before a measurement. A desktop reports `AC_LINE_ONLINE`, which is the answer we want: not on
/// battery.
pub(super) fn on_battery() -> Option<bool> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a correctly sized, zeroed structure that the call may write to, and it
    // outlives the call.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return None;
    }
    match status.ACLineStatus {
        AC_LINE_OFFLINE => Some(true),
        AC_LINE_UNKNOWN => None,
        _ => Some(false),
    }
}

fn apply(mode: i32, verb: &str) -> Capability {
    // SAFETY: GetCurrentThread returns a pseudo-handle to the calling thread that needs no closing,
    // and `mode` is one of the documented THREAD_PRIORITY constants.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), mode) };
    if ok != 0 {
        Capability::Applied
    } else {
        // SAFETY: called immediately after the failed call on the same thread.
        let error = unsafe { GetLastError() };
        Capability::Unsupported(format!(
            "SetThreadPriority could not {verb} background mode (os error {error})"
        ))
    }
}
