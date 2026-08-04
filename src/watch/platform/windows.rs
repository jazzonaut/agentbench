//! Windows implementations: `%LOCALAPPDATA%`, `LockFileEx`, per-thread background mode, and PDH.

use super::{Capability, CounterReading};
use anyhow::{Context, Result, bail};
use std::{env, ffi::c_void, fs::File, os::windows::io::AsRawHandle, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_LOCK_VIOLATION, GetLastError, HANDLE},
    Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
    Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx},
    System::{
        IO::OVERLAPPED,
        Performance::{
            PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
            PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
            PdhOpenQueryW,
        },
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
        Threading::{
            GetCurrentProcess, GetCurrentThread, OpenProcessToken, SetThreadPriority,
            THREAD_MODE_BACKGROUND_BEGIN,
        },
    },
};

/// `ACLineStatus` value meaning "running on battery".
const AC_LINE_OFFLINE: u8 = 0;

/// `ACLineStatus` value meaning "the system cannot tell".
const AC_LINE_UNKNOWN: u8 = 255;

/// Clock as a percentage of nominal, across every core.
///
/// `Processor Information` rather than the older `Processor` object: the latter has no equivalent counter
/// and stops at 64 logical processors.
const CLOCK_COUNTER: &str = r"\Processor Information(_Total)\% Processor Performance";

/// Whole-machine disk write throughput, by every process including the ones this token cannot open.
const DISK_WRITE_COUNTER: &str = r"\PhysicalDisk(_Total)\Disk Write Bytes/sec";

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

/// Whether this process is running elevated, from its own token.
///
/// `TokenElevation` is the question UAC actually answers: it is true for a process running with a full
/// administrator token, and false for the filtered token an administrator's ordinary session gets. That is
/// the distinction the elevated diagnostics need, and it is one call plus a handle rather than the `net
/// session` child process this replaced.
pub(super) fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the current-process pseudo-handle needs no closing, and `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0_u32;
    // SAFETY: `elevation` outlives the call and its declared length matches the type the information
    // class returns; `returned` is a valid out-pointer.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: `token` was opened by the call above and is not used again.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// A PDH query and the counters added to it.
///
/// Raw handles rather than a wrapper type because `windows-sys` is a bindings crate and provides none.
/// Not `Send`, which is correct and enforced by the pointers: the prober constructs this on its own
/// thread and nothing else ever touches it.
pub(super) struct Counters {
    /// `PDH_HQUERY`, or null when the query could not be opened at all.
    query: *mut c_void,
    /// `PDH_HCOUNTER` per counter, absent where that individual counter was refused.
    ///
    /// Independently optional rather than all-or-nothing: a machine with the disk object disabled in
    /// `HKLM\SYSTEM\CurrentControlSet\Services\PhysicalDisk` should still report its clock.
    clock: Option<*mut c_void>,
    disk_write: Option<*mut c_void>,
}

impl Counters {
    pub(super) fn open() -> (Self, Capability) {
        let mut query: *mut c_void = std::ptr::null_mut();
        // SAFETY: a null data source means "live data", and `query` is a valid out-pointer that
        // outlives the call.
        let status = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if status != 0 {
            return (
                Self {
                    query: std::ptr::null_mut(),
                    clock: None,
                    disk_write: None,
                },
                Capability::Unsupported(format!(
                    "PdhOpenQueryW failed (0x{status:08X}); clock and disk conditions are unavailable"
                )),
            );
        }

        let (clock, clock_error) = add(query, CLOCK_COUNTER);
        let (disk_write, disk_error) = add(query, DISK_WRITE_COUNTER);
        let counters = Self {
            query,
            clock,
            disk_write,
        };
        let refused: Vec<String> = [clock_error, disk_error].into_iter().flatten().collect();
        let capability = if refused.is_empty() {
            Capability::Applied
        } else {
            Capability::Unsupported(refused.join("; "))
        };
        (counters, capability)
    }

    pub(super) fn prime(&mut self) {
        self.collect();
    }

    pub(super) fn read(&mut self) -> CounterReading {
        self.collect();
        CounterReading {
            clock_percent: self.value(self.clock).map(|value| value as f32),
            disk_write_bytes_s: self.value(self.disk_write),
        }
    }

    /// Sample every counter on the query into PDH's own two-deep history.
    fn collect(&self) {
        if self.query.is_null() {
            return;
        }
        // SAFETY: `query` was returned by `PdhOpenQueryW` and is closed only by `Drop`.
        unsafe { PdhCollectQueryData(self.query) };
    }

    /// The formatted value of one counter, or `None` where PDH has no usable reading.
    ///
    /// A failure is not logged. `PDH_INVALID_DATA` is the *expected* answer after the first collect of a
    /// rate counter, and a daemon that warned about it would warn once per probe for ever.
    ///
    /// **Two places report failure and both have to be read.** `PdhGetFormattedCounterValue` can return
    /// `ERROR_SUCCESS` while putting the real answer in the structure's `CStatus` field, in which case
    /// `doubleValue` holds nothing meaningful. The plausibility guard below would let a garbage *zero*
    /// through, and zero is the one value this module must never invent: a disk rate of zero is a claim
    /// that the machine was quiet, which is exactly what a busy one would then look like.
    fn value(&self, counter: Option<*mut c_void>) -> Option<f64> {
        let counter = counter?;
        let mut value: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
        // SAFETY: `counter` belongs to a query still open, the format constant is a documented one, and
        // `value` is a correctly sized structure that outlives the call. The type out-parameter is
        // optional and passed as null.
        let status = unsafe {
            PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut value)
        };
        if status != 0 {
            return None;
        }
        if !matches!(value.CStatus, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA) {
            return None;
        }
        // SAFETY: the union's `doubleValue` arm is the one `PDH_FMT_DOUBLE` fills in.
        let reading = unsafe { value.Anonymous.doubleValue };
        // A counter can report a negative rate across a wrap; absent is the honest reading, not zero.
        reading.is_finite().then_some(reading).filter(|v| *v >= 0.0)
    }
}

impl Drop for Counters {
    fn drop(&mut self) {
        if self.query.is_null() {
            return;
        }
        // SAFETY: closing the query releases its counters too; nothing here is used afterwards.
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// Add one counter to a query, by its **English** path.
///
/// `PdhAddEnglishCounterW`, never `PdhAddCounterW`: a counter path is localised, so `\PhysicalDisk` is
/// `\Physikalischer Datenträger` on a German install and a by-name lookup there fails at runtime — on a
/// machine no test of ours will ever run on.
fn add(query: *mut c_void, path: &str) -> (Option<*mut c_void>, Option<String>) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut counter: *mut c_void = std::ptr::null_mut();
    // SAFETY: `wide` is nul-terminated and outlives the call; `counter` is a valid out-pointer.
    let status = unsafe { PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) };
    if status == 0 {
        (Some(counter), None)
    } else {
        (
            None,
            Some(format!("PDH counter {path} unavailable (0x{status:08X})")),
        )
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
