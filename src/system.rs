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

/// Stable, hashed identity of this machine.
///
/// Extracted so that a caller needing only the identity does not have to build a whole [`Inventory`],
/// which enumerates every disk and spawns a child process to name the power source. [`inventory`] uses
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

/// Refresh exactly what [`sample_from`] and a process-tree walk read, and nothing else.
///
/// Separate from the reading so a caller that already walks the process table for its own reasons can
/// do it once. The enumeration is the expensive part — on Windows it is the most expensive thing this
/// tool does per unit time — so it is worth both not repeating it and not asking for more than is
/// wanted: `refresh_all` would additionally fetch each process's command line, environment and owner,
/// none of which anything here reads.
pub fn refresh_for_sample(system: &mut System) {
    system.refresh_memory();
    system.refresh_cpu_usage();
    system.refresh_processes(ProcessesToUpdate::All, true);
}

/// Read a sample from an already-refreshed `System`.
pub fn sample_from(system: &System, started: Instant) -> SystemSample {
    let scanner_names = [
        "msmpeng",
        "windefend",
        "sophos",
        "crowdstrike",
        "sentinelone",
        "clamd",
        "eset",
        "avast",
        "avg",
    ];
    let scanner_cpu: f32 = system
        .processes()
        .values()
        .filter(|p| {
            let name = p.name().to_string_lossy().to_ascii_lowercase();
            scanner_names.iter().any(|scanner| name.contains(scanner))
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

#[cfg(windows)]
fn is_elevated() -> bool {
    Command::new("net")
        .arg("session")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn is_elevated() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

#[cfg(windows)]
fn power_source() -> Option<String> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue).BatteryStatus",
        ])
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Some("ac_or_desktop".into())
    } else {
        Some(format!("battery_status_{value}"))
    }
}

/// Which source is powering the machine, named for the report.
///
/// Delegated rather than reimplemented. There were two Unix readings of this one fact and they
/// disagreed: this one walked `/sys/class/power_supply` looking for any `online` file, and since a
/// battery has no such file and `read_dir` order is arbitrary, visiting `BAT0` first returned `None`
/// for the whole function — the answer was decided by directory ordering. The watch-side reading
/// filters on `type == "Mains"`, keeps looking, and distinguishes "cannot tell" from "on mains", so it
/// is the one that survives.
///
/// The cost argument that kept them apart no longer holds either way round: the surviving
/// implementation is a few small reads on Linux and one short-lived `pmset` on macOS, which is what
/// this path was already spending.
#[cfg(unix)]
fn power_source() -> Option<String> {
    Some(match crate::watch::platform::on_battery()? {
        true => "battery".into(),
        false => "ac".into(),
    })
}

#[cfg(not(any(windows, unix)))]
fn power_source() -> Option<String> {
    None
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
