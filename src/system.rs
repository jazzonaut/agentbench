use crate::model::{DiskInfo, Inventory, SystemSample};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, env, fs, path::Path, process::Command, time::Instant};
use sysinfo::{Disks, ProcessesToUpdate, System};

pub fn hash_private(value: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(value.as_ref());
    hex::encode(&digest[..8])
}

pub fn redact_text(value: &str) -> String {
    let mut output = value.to_string();
    if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = home.to_string_lossy();
        output = output.replace(home.as_ref(), "<home>");
        output = output.replace(&home.replace('\\', "/"), "<home>");
    }
    output
}

/// Free space on the most specific mounted volume containing `path`.
///
/// `None` when no mount point matches, which every caller treats as unknown rather than as empty.
///
/// Extracted for the same reason [`machine_id`] was: two callers now need it and they must agree. The
/// benchmark asks before it writes, to refuse a run that would fill the volume; the prober asks on every
/// probe, because both NTFS and SSDs slow down as a volume fills and that is the one cause of a slow
/// monotonic drift the dashboard could not previously explain. "Most specific mount point" is the part
/// worth sharing — on Windows the match is a drive letter, but on Unix a nested mount means the longest
/// matching prefix is the only correct answer.
pub fn available_space(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

/// Stable, hashed identity of this machine.
///
/// Extracted so that a caller needing only the identity does not have to build a whole [`Inventory`],
/// which enumerates every disk and every process on the machine. [`inventory`] uses
/// it too, so the two can never disagree about which machine a row belongs to — and they must not, since
/// this value is the primary key of the dashboard's `machines` table.
pub fn machine_id() -> String {
    hash_private(System::host_name().unwrap_or_else(|| "unknown".into()))
}

pub fn inventory(elevated_requested: bool) -> Inventory {
    let mut system = System::new_all();
    system.refresh_all();
    let disks = Disks::new_with_refreshed_list();
    let cpu = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let mut result = Inventory {
        os: System::name().unwrap_or_else(|| env::consts::OS.into()),
        os_version: System::long_os_version()
            .or_else(System::os_version)
            .unwrap_or_else(|| "unknown".into()),
        architecture: env::consts::ARCH.into(),
        hostname_hash: machine_id(),
        cpu,
        // An associated function since sysinfo 0.34: the physical count comes from the OS directly
        // and needs no refreshed `System`.
        physical_cores: System::physical_core_count(),
        logical_cores: system.cpus().len(),
        memory_bytes: system.total_memory(),
        disks: disks
            .iter()
            .map(|disk| DiskInfo {
                name: hash_private(disk.name().to_string_lossy().as_bytes()),
                kind: format!("{:?}", disk.kind()),
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
                filesystem: disk.file_system().to_string_lossy().into_owned(),
                removable: disk.is_removable(),
            })
            .collect(),
        power_source: power_source(),
        elevated: is_elevated(),
        tool_versions: BTreeMap::new(),
        config_fingerprints: config_fingerprints(),
    };
    if elevated_requested && !result.elevated {
        result
            .config_fingerprints
            .insert("elevation".into(), "requested_but_unavailable".into());
    }
    result
}

/// Refresh the whole-machine counters, which are cheap.
///
/// Split from [`refresh_processes_for_sample`] because the two have very different costs and a
/// sampler wants them on different cadences: memory and CPU are a couple of reads, and the process
/// walk is measured in milliseconds.
pub fn refresh_machine(system: &mut System) {
    system.refresh_memory();
    system.refresh_cpu_usage();
}

/// Re-enumerate the process table, which is the expensive half of a sample.
///
/// On Windows this is the most expensive thing this tool does per unit time: measured at 9.9 ms mean
/// and 13.8 ms worst case over 365 processes, which is why the sampler runs it on a slower cadence
/// than the reading itself. `refresh_all` would additionally fetch each process's command line,
/// environment and owner, none of which anything here reads.
pub fn refresh_processes_for_sample(system: &mut System) {
    system.refresh_processes(ProcessesToUpdate::All, true);
}

/// Refresh exactly what [`sample_from`] and a process-tree walk read, and nothing else.
pub fn refresh_for_sample(system: &mut System) {
    refresh_machine(system);
    refresh_processes_for_sample(system);
}

/// Read a sample from an already-refreshed `System`.
///
/// `scanner_cpu_percent` is on the per-core scale documented at
/// [`process_tree::TreeUsage::cpu_percent`], unlike `cpu_percent` beside it, and reads `0.0` until the
/// process table has been refreshed three times.
pub fn sample_from(system: &System, started: Instant) -> SystemSample {
    let scanner_cpu: f32 = system
        .processes()
        .values()
        .filter(|p| {
            let name = p.name().to_string_lossy().to_ascii_lowercase();
            crate::watch::config::SCANNER_NAME_FRAGMENTS
                .iter()
                .any(|scanner| name.contains(scanner))
        })
        .map(|p| p.cpu_usage())
        .sum();
    SystemSample {
        elapsed_ms: started.elapsed().as_millis() as u64,
        cpu_percent: system.global_cpu_usage(),
        used_memory_bytes: system.used_memory(),
        used_swap_bytes: system.used_swap(),
        process_count: system.processes().len(),
        scanner_cpu_percent: (scanner_cpu > 0.0).then_some(scanner_cpu),
    }
}

/// Refresh and read in one step, for a caller with no other use for the `System`.
pub fn sample(system: &mut System, started: Instant) -> SystemSample {
    refresh_for_sample(system);
    sample_from(system, started)
}

pub fn tool_version(program: &str, args: &[&str]) -> Option<(String, u64)> {
    let started = Instant::now();
    let output = Command::new(program).args(args).output().ok()?;
    let elapsed = started.elapsed().as_millis() as u64;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    Some((
        text.lines().next().unwrap_or_default().trim().to_string(),
        elapsed,
    ))
}

pub fn path_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(hash_private(format!("{}:{modified}", metadata.len())))
}

fn config_fingerprints() -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(std::path::PathBuf::from);
    if let Some(home) = home {
        for (name, relative) in [
            ("claude_settings", ".claude/settings.json"),
            ("headroom_config", ".headroom/config.toml"),
            ("tokensave_db", ".tokensave/tokensave.db"),
        ] {
            if let Some(value) = path_fingerprint(&home.join(relative)) {
                result.insert(name.into(), value);
            }
        }
    }
    for name in [
        "ANTHROPIC_BASE_URL",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "HEADROOM_PORT",
        "CLAUDE_CONFIG_DIR",
    ] {
        if env::var_os(name).is_some() {
            result.insert(format!("env:{name}"), "present".into());
        }
    }
    result
}

/// Whether this process holds administrative privileges.
///
/// Delegated rather than reimplemented, for the same reason [`power_source`] is. Both readings this
/// replaced spawned a child process — `net session` and `id -u` — to answer a question the process can ask
/// about itself, on a path `inventory()` runs on every invocation.
fn is_elevated() -> bool {
    crate::watch::platform::is_elevated()
}

/// Which source is powering the machine, named for the report.
///
/// Delegated rather than reimplemented, on every platform. There were two Unix readings of this one fact
/// and they disagreed: this one walked `/sys/class/power_supply` looking for any `online` file, and since a
/// battery has no such file and `read_dir` order is arbitrary, visiting `BAT0` first returned `None`
/// for the whole function — the answer was decided by directory ordering. The watch-side reading
/// filters on `type == "Mains"`, keeps looking, and distinguishes "cannot tell" from "on mains", so it
/// is the one that survives.
///
/// The cost argument that kept them apart no longer holds either way round: the surviving
/// implementation is a few small reads on Linux and one short-lived `pmset` on macOS, which is what
/// this path was already spending.
///
/// Windows followed later, for a reason ADR 0001 could not have anticipated when it permitted a child
/// process here. This arm spent a `powershell -Command (Get-CimInstance Win32_Battery …)` — a WMI query,
/// several hundred milliseconds — on a question `GetSystemPowerStatus` answers in one call, and the ADR
/// allowed it because `inventory()` runs once per report and a report is printed to a console. The tray
/// build has no console, so that child arrived as a PowerShell window flashing on the user's desktop at
/// every login.
///
/// The recorded string changes with it: Windows reported `ac_or_desktop` or `battery_status_2`, and now
/// reports the `ac`/`battery` every other platform already used. Nothing reads it but the JSON report, and
/// one spelling across platforms is what a reader holding two reports needs. A desktop still reads `ac`:
/// `GetSystemPowerStatus` reports `AC_LINE_ONLINE` for a machine with no battery at all.
fn power_source() -> Option<String> {
    Some(match crate::watch::platform::on_battery()? {
        true => "battery".into(),
        false => "ac".into(),
    })
}

pub fn native_diagnostics(_elevated_requested: bool) -> (serde_json::Value, Vec<String>) {
    let mut unavailable = Vec::new();
    // Only Windows and Linux have diagnostics to insert here. macOS contributes to `unavailable`
    // instead, and so never mutates this map.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(unused_mut))]
    let mut data = serde_json::Map::new();
    #[cfg(windows)]
    {
        if _elevated_requested && is_elevated() {
            let script = "Get-MpComputerStatus | Select-Object AntivirusEnabled,RealTimeProtectionEnabled,BehaviorMonitorEnabled,IoavProtectionEnabled | ConvertTo-Json -Compress";
            match Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .output()
            {
                Ok(out) if out.status.success() => {
                    if let Ok(value) = serde_json::from_slice(&out.stdout) {
                        data.insert("windows_defender".into(), value);
                    }
                }
                _ => unavailable.push("Windows Defender status".into()),
            }
        } else if _elevated_requested {
            unavailable.push(
                "elevated Windows security diagnostics (start from an Administrator terminal)"
                    .into(),
            );
        } else {
            unavailable.push(
                "Windows Defender details (rerun with --elevated from an Administrator terminal)"
                    .into(),
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        for (name, path) in [
            ("cpu_pressure", "/proc/pressure/cpu"),
            ("io_pressure", "/proc/pressure/io"),
            ("memory_pressure", "/proc/pressure/memory"),
        ] {
            if let Ok(text) = fs::read_to_string(path) {
                data.insert(name.into(), serde_json::Value::String(text.trim().into()));
            } else {
                unavailable.push(name.replace('_', " "));
            }
        }
    }
    #[cfg(target_os = "macos")]
    unavailable.push("macOS thermal pressure detail (requires powermetrics privileges)".into());
    (serde_json::Value::Object(data), unavailable)
}
