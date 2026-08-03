//! Unix implementations: XDG/Application Support paths, `flock`, and nice/idle-I/O scheduling.

use super::Capability;
use anyhow::{Context, Result, bail};
use std::{env, fs::File, os::unix::io::AsRawFd, path::PathBuf};

/// Nice increment applied to background collector threads.
///
/// Linux only: it is the value for `PRIO_PROCESS`, which is per-thread there and nowhere else.
#[cfg(target_os = "linux")]
const BACKGROUND_NICE: libc::c_int = 10;

/// Linux's power-supply class, where mains adapters advertise whether they are supplying power.
#[cfg(target_os = "linux")]
const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";

pub(super) fn default_data_dir() -> Result<PathBuf> {
    if cfg!(target_os = "macos")
        && let Some(home) = env::var_os("HOME")
    {
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support"));
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

/// Lower scheduling priority for the calling thread, or refuse to touch anything.
///
/// The granularity is the whole point, and it is not portable. On Linux `setpriority(PRIO_PROCESS, 0,
/// …)` applies to the calling *thread*; on macOS and the BSDs the same call applies to the whole
/// *process*. Treating that as a harmless degradation was a mistake worth spelling out: the daemon runs
/// the sampler in the background and the prober at normal priority deliberately, and a process-wide
/// nice applied by the sampler drags the prober down with it. The prober would then be measuring its
/// own throttle — the exact failure [`probes`] documents as forbidden — and would do so invisibly,
/// because the bias is constant and a day-over-day baseline absorbs it while contended-versus-clean
/// interpretation quietly inflates.
///
/// So each platform gets the call that means what this function claims, and a platform with no such
/// call reports [`Capability::Unsupported`] rather than throttling a thread it was not asked to touch.
/// The sampler then competes at normal priority and says so in the log, which is the lesser harm.
///
/// Not reversible anywhere: `setpriority` will not lower a nice value again without privileges, which
/// is why no counterpart exists.
///
/// [`probes`]: crate::watch::collect::probes
#[cfg(target_os = "linux")]
pub(super) fn set_current_thread_background() -> Capability {
    set_nice(libc::PRIO_PROCESS, BACKGROUND_NICE, "setpriority")
}

/// macOS has the exact analogue: per-thread, and it throttles I/O as well as CPU.
///
/// `PRIO_DARWIN_THREAD` with `who = 0` is defined to affect the calling thread alone, and
/// `PRIO_DARWIN_BG` puts it in the background band the OS uses for its own housekeeping — which is
/// what `THREAD_MODE_BACKGROUND_BEGIN` does on Windows, and therefore what the daemon means by
/// "background" everywhere.
#[cfg(target_os = "macos")]
pub(super) fn set_current_thread_background() -> Capability {
    set_nice(
        libc::PRIO_DARWIN_THREAD,
        libc::PRIO_DARWIN_BG,
        "setpriority(PRIO_DARWIN_THREAD)",
    )
}

/// Every other Unix declines, because the only call available would reach the prober.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(super) fn set_current_thread_background() -> Capability {
    Capability::Unsupported(
        "setpriority is process-wide on this platform, and lowering it would throttle the probe \
         thread as well"
            .into(),
    )
}

/// Whether the machine is on battery, from sysfs.
///
/// A few small reads and no child process, which is what makes it affordable immediately before a
/// measurement. Several mains supplies can be present at once — a charger and a dock — so any one of
/// them supplying power settles the question. Finding no readable mains supply at all reports "cannot
/// tell" rather than "on mains": that is what a container or an unusual kernel looks like, and a probe
/// stamped with a guess is worse than one stamped with nothing.
#[cfg(target_os = "linux")]
pub(super) fn on_battery() -> Option<bool> {
    let entries = std::fs::read_dir(POWER_SUPPLY_DIR).ok()?;
    let mut offline = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = std::fs::read_to_string(path.join("type")) else {
            continue;
        };
        if kind.trim() != "Mains" {
            continue;
        }
        match std::fs::read_to_string(path.join("online"))
            .as_deref()
            .map(str::trim)
        {
            Ok("1") => return Some(false),
            Ok("0") => offline = Some(true),
            _ => {}
        }
    }
    offline
}

/// Whether the machine is on battery, from `pmset`.
///
/// macOS exposes this through IOKit and nowhere cheaper, so this spends a short-lived child process
/// rather than adding a dependency for one boolean. Four times an hour is affordable; it is asked
/// before the workloads run, never between them. `-g ps` rather than `-g batt` because it answers on
/// a desktop with no battery too.
#[cfg(target_os = "macos")]
pub(super) fn on_battery() -> Option<bool> {
    let output = std::process::Command::new("/usr/bin/pmset")
        .args(["-g", "ps"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if text.contains("'ac power'") {
        return Some(false);
    }
    if text.contains("'battery power'") {
        return Some(true);
    }
    None
}

/// Every other Unix reports that it cannot tell, which is true.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn on_battery() -> Option<bool> {
    None
}

/// Root is euid 0, which the process can read about itself.
///
/// The effective uid rather than the real one: it is what the kernel checks, so a setuid binary's answer
/// matches what it will actually be allowed to do.
pub(super) fn is_elevated() -> bool {
    // SAFETY: geteuid cannot fail, takes no arguments and touches no memory.
    unsafe { libc::geteuid() == 0 }
}

/// Apply one `setpriority` call to the caller, naming it for the refusal message.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_nice(which: libc::c_int, value: libc::c_int, called: &str) -> Capability {
    // SAFETY: setpriority with who=0 targets the caller and takes no pointers.
    let result = unsafe { libc::setpriority(which, 0, value) };
    if result == 0 {
        Capability::Applied
    } else {
        Capability::Unsupported(format!(
            "{called} with {value} was refused: {}",
            std::io::Error::last_os_error()
        ))
    }
}
