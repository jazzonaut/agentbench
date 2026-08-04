//! Unix implementations: XDG/Application Support paths, `flock`, nice/idle-I/O scheduling, and the
//! sysfs/procfs counters Linux exposes for free.

use super::{Capability, CounterReading};
use anyhow::{Context, Result, bail};
use std::{env, fs::File, os::unix::io::AsRawFd, path::PathBuf};

/// Nice increment applied to background collector threads.
///
/// Linux only: it is the value for `PRIO_PROCESS`, which is per-thread there and nowhere else.
#[cfg(target_os = "linux")]
const BACKGROUND_NICE: libc::c_int = 10;

/// The type this platform's `setpriority` takes for its first argument.
///
/// Not `c_int` everywhere, which is what this used to assume. glibc declares the parameter as
/// `__priority_which_t`, an enum, and `libc` 0.2.189 started reflecting that as `u32` — so a signature
/// fixed at `c_int` stopped compiling on `x86_64-unknown-linux-gnu` while remaining correct on macOS and
/// on musl, both of which declare a plain `int`. An alias rather than a cast at the call site, so the
/// mismatch is impossible rather than papered over, and so a future divergence shows up here.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
type PriorityWhich = u32;

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    not(all(target_os = "linux", target_env = "gnu"))
))]
type PriorityWhich = libc::c_int;

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

/// Live conditions from procfs and sysfs, both of which are world-readable.
///
/// No handle to open and nothing to close, unlike the Windows counterpart: what has to be remembered is
/// the opening total, because `/proc/diskstats` is cumulative and the rate is ours to compute.
#[cfg(target_os = "linux")]
pub(super) struct Counters {
    /// Sectors written across every whole device, and when that was read.
    opening: Option<(u64, std::time::Instant)>,
}

#[cfg(target_os = "linux")]
impl Counters {
    pub(super) fn open() -> (Self, Capability) {
        let counters = Self { opening: None };
        // Both readings are plain file reads, so the only thing worth reporting is whether the files a
        // container or an unusual kernel might not have are actually there.
        let capability = match (
            std::path::Path::new(DISKSTATS).exists(),
            clock_percent().is_some(),
        ) {
            (true, true) => Capability::Applied,
            (disk, clock) => Capability::Unsupported(format!(
                "{} unavailable{}",
                if disk { "cpufreq" } else { DISKSTATS },
                if disk || clock {
                    ""
                } else {
                    " and cpufreq is absent too"
                }
            )),
        };
        (counters, capability)
    }

    pub(super) fn prime(&mut self) {
        self.opening = sectors_written().map(|total| (total, std::time::Instant::now()));
    }

    pub(super) fn read(&mut self) -> CounterReading {
        let closing = sectors_written().map(|total| (total, std::time::Instant::now()));
        let rate = match (self.opening, closing) {
            (Some((before, at)), Some((after, now))) => {
                let seconds = now.duration_since(at).as_secs_f64();
                // A counter that went backwards is a device that was removed, not negative throughput.
                (seconds > 0.0 && after >= before)
                    .then(|| (after - before) as f64 * SECTOR_BYTES as f64 / seconds)
            }
            _ => None,
        };
        self.opening = closing;
        CounterReading {
            clock_percent: clock_percent(),
            disk_write_bytes_s: rate,
        }
    }
}

/// Every other Unix reports nothing, which is true rather than convenient.
///
/// macOS has no equivalent it can be asked cheaply: the disk figure lives behind IOKit, which would mean a
/// new dependency for one covariate, and the CPU's live clock is not exposed to an ordinary process at
/// all. A guessed clock would be the worst outcome — see [`CounterReading::clock_percent`].
#[cfg(not(target_os = "linux"))]
pub(super) struct Counters;

#[cfg(not(target_os = "linux"))]
impl Counters {
    pub(super) fn open() -> (Self, Capability) {
        (
            Self,
            Capability::Unsupported(
                "this platform exposes neither a live clock ratio nor whole-machine disk throughput \
                 to an unprivileged process"
                    .into(),
            ),
        )
    }

    pub(super) fn prime(&mut self) {}

    pub(super) fn read(&mut self) -> CounterReading {
        CounterReading::default()
    }
}

/// Kernel-reported block I/O totals. Cumulative, so a rate is the caller's to compute.
#[cfg(target_os = "linux")]
const DISKSTATS: &str = "/proc/diskstats";

/// `/proc/diskstats` counts sectors, and the unit there is fixed at 512 bytes regardless of the
/// device's real sector size. Not `logical_block_size`: the kernel normalises before reporting.
#[cfg(target_os = "linux")]
const SECTOR_BYTES: u64 = 512;

/// Sectors written across every block device that nothing else lies beneath.
///
/// Two kinds of double counting to avoid, and `/sys/block/<name>` alone only prevents the first.
///
/// A **partition** does not appear there at all — `sda1` lives at `/sys/block/sda/sda1` — so the existence
/// check drops partitions, which is what it was written for: a partition's writes are also its disk's.
///
/// A **stacked** device does appear there. `/sys/block/dm-0` and `/sys/block/md0` both exist, so a write
/// through LVM, LUKS, RAID or devicemapper was counted once for the mapping and again for the disk under
/// it, doubling the reported rate. That is not an exotic configuration: it is the Ubuntu installer's
/// default and it is how most container hosts are built, which made this wrong on the machines least able
/// to check it. A doubled rate matters because the figure is compared against a threshold — 20 MiB/s of
/// contention would have fired at 10 MiB/s of real traffic, quietly shrinking the comparable subset.
///
/// `/sys/block/<name>/slaves` is the kernel's own answer to "does anything lie beneath this": it is
/// populated for exactly the stacked devices and empty for physical ones, so no list of name prefixes has
/// to be maintained here as the kernel gains new mapping types.
#[cfg(target_os = "linux")]
fn sectors_written() -> Option<u64> {
    let text = std::fs::read_to_string(DISKSTATS).ok()?;
    let mut total = 0_u64;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // major, minor, name, then reads completed, merged, sectors and milliseconds, then writes
        // completed and merged, which puts sectors written at index 9.
        let (Some(name), Some(written)) = (fields.get(2), fields.get(9)) else {
            continue;
        };
        let device = std::path::Path::new("/sys/block").join(name);
        if !device.exists() || stacks_on_another_device(&device) {
            continue;
        }
        total = total.saturating_add(written.parse::<u64>().unwrap_or(0));
    }
    Some(total)
}

/// Whether this block device is a mapping over other devices rather than one of its own.
///
/// An unreadable `slaves` directory is treated as "physical", which is the reading that risks a missing
/// write rather than a doubled one. Under-reporting a rate costs a probe that should have been tagged
/// contended; over-reporting it costs every clean probe on the machine.
#[cfg(target_os = "linux")]
fn stacks_on_another_device(device: &std::path::Path) -> bool {
    std::fs::read_dir(device.join("slaves"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Current clock as a percentage of the maximum this CPU advertises, averaged over the cores that say.
///
/// `scaling_cur_freq` against `cpuinfo_max_freq`, both in kHz. Absent where cpufreq is not built or not
/// exposed, which is what a virtual machine usually looks like. Note the ceiling differs from the Windows
/// counterpart's: this is a percentage of *maximum*, so it reaches 100 rather than exceeding it, and the
/// two are therefore comparable in direction and in relative movement but not in absolute value. That is
/// stated here because nothing downstream can discover it.
#[cfg(target_os = "linux")]
fn clock_percent() -> Option<f32> {
    let root = std::path::Path::new("/sys/devices/system/cpu");
    let mut ratios = Vec::new();
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let cpufreq = entry.path().join("cpufreq");
        let read = |name: &str| -> Option<f64> {
            std::fs::read_to_string(cpufreq.join(name))
                .ok()?
                .trim()
                .parse::<f64>()
                .ok()
        };
        let (Some(current), Some(max)) = (read("scaling_cur_freq"), read("cpuinfo_max_freq"))
        else {
            continue;
        };
        if max > 0.0 {
            ratios.push(current / max * 100.0);
        }
    }
    if ratios.is_empty() {
        return None;
    }
    Some((ratios.iter().sum::<f64>() / ratios.len() as f64) as f32)
}

/// Apply one `setpriority` call to the caller, naming it for the refusal message.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_nice(which: PriorityWhich, value: libc::c_int, called: &str) -> Capability {
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
